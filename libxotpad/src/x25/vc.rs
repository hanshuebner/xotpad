//! X.25 virtual circuits.
//!
//! This module provides functionality for handling X.25 virtual circuits.

use bytes::{BufMut, Bytes, BytesMut};
use std::cmp::min;
use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant};
use tracing_mutex::stdsync::{Condvar, Mutex, RwLock};

use crate::x121::X121Addr;
use crate::x25::facility::X25Facility;
use crate::x25::packet::{
    X25CallAccept, X25CallRequest, X25ClearConfirm, X25ClearRequest, X25Data, X25Packet,
    X25ReceiveReady, X25ResetConfirm, X25ResetRequest,
};
use crate::x25::params::X25Params;
use crate::x25::seq::{next_seq, Window, X25Modulo};
use crate::xot::XotLink;

/// X.25 virtual circuit.
pub trait Vc {
    fn send(&self, user_data: Bytes, qualifier: bool) -> io::Result<()>;

    fn recv(&self) -> io::Result<Option<(Bytes, bool)>>;

    fn reset(&self, cause_code: u8, diagnostic_code: u8) -> io::Result<()>;

    fn flush(&self) -> io::Result<()>;

    fn params(&self) -> X25Params;

    fn is_connected(&self) -> bool;
}

#[derive(Debug)]
enum VcState {
    Ready,
    WaitCallAccept(Instant),
    DataTransfer(DataTransferState),
    WaitResetConfirm(Instant),
    WaitClearConfirm(Instant, ClearInitiator),

    // These are our custom ones...
    Called(X25CallRequest),
    #[allow(dead_code)] // TODO
    Cleared(ClearInitiator, Option<X25ClearConfirm>),
    OutOfOrder,
}

impl VcState {
    fn is_connected(&self) -> bool {
        matches!(
            self,
            VcState::DataTransfer(_) | VcState::WaitResetConfirm(_)
        )
    }
}

#[derive(Debug)]
struct DataTransferState {
    modulo: X25Modulo,
    send_window: Window,
    recv_seq: u8,
}

#[derive(Clone, Debug)]
enum ClearInitiator {
    Local,
    Remote(X25ClearRequest),
    #[allow(dead_code)] // TODO
    TimeOut(u8),
}

/// X.25 _switched_ virtual circuit, or _virtual call_.
pub struct Svc(Arc<VcInner>);

impl Svc {
    pub fn call(
        link: XotLink,
        channel: u16,
        addr: &X121Addr,
        call_user_data: &[u8],
        params: &X25Params,
    ) -> io::Result<Self> {
        let svc = Svc::new(link, channel, params);

        {
            let inner = &svc.0;

            // Send the call request packet.
            {
                let mut state = inner.state.0.lock().unwrap();

                if !matches!(*state, VcState::Ready) {
                    todo!("invalid state");
                }

                let call_request = create_call_request(channel, addr, call_user_data, params);

                if let Err(err) = inner.send_packet(&call_request.into()) {
                    inner.out_of_order(&mut state, err);
                    inner.engine_wait.notify_all();
                    return Err(io::Error::other("link is out of order"));
                }

                let next_state = VcState::WaitCallAccept(Instant::now());

                inner.change_state(&mut state, next_state);
                inner.engine_wait.notify_all();
            }

            // Wait for the result.
            let mut state = inner.state.0.lock().unwrap();

            while matches!(*state, VcState::WaitCallAccept(_)) {
                state = inner.state.1.wait(state).unwrap();
            }

            // Consider the call a success if there is any data, irrespective of
            // the current state. If the remote party sends data and immediately
            // sends a clear request, then the call may already be cleared but
            // the data will be lost if we don't return the call to the client.
            let queue = inner.recv_data_queue.0.lock().unwrap();

            if queue.is_empty() && !state.is_connected() {
                match *state {
                    VcState::Cleared(ClearInitiator::Remote(ref clear_request), _) => {
                        let X25ClearRequest {
                            cause_code,
                            diagnostic_code,
                            ..
                        } = clear_request;
                        let msg = format!("C:{cause_code} D:{diagnostic_code}");
                        return Err(io::Error::new(io::ErrorKind::ConnectionReset, msg));
                    }
                    VcState::WaitClearConfirm(_, ClearInitiator::TimeOut(_))
                    | VcState::Cleared(ClearInitiator::TimeOut(_), _) => {
                        return Err(io::Error::from(io::ErrorKind::TimedOut));
                    }
                    VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
                    _ => panic!("unexpected state"),
                }
            }
        }

        Ok(svc)
    }

    pub fn listen_timeout(
        link: XotLink,
        channel: u16,
        params: &X25Params,
        duration: Duration,
    ) -> io::Result<SvcIncomingCall> {
        let svc = Svc::new(link, channel, params);

        let call_request = {
            let inner = &svc.0;

            let mut state = inner.state.0.lock().unwrap();

            let mut remaining_duration = duration;

            while matches!(*state, VcState::Ready) {
                let start = Instant::now();

                let result = inner
                    .state
                    .1
                    .wait_timeout(state, remaining_duration)
                    .unwrap();

                state = result.0;

                if !matches!(*state, VcState::Ready) {
                    break;
                }

                remaining_duration = remaining_duration.saturating_sub(start.elapsed());

                if result.1.timed_out() || remaining_duration.is_zero() {
                    // TODO: See note below about returning XotLink to the caller...
                    let _ = inner.send_link.lock().unwrap().shutdown();

                    return Err(io::Error::from(io::ErrorKind::TimedOut));
                }
            }

            match *state {
                VcState::Called(ref call_request) => call_request.clone(),
                VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
                _ => panic!("unexpected state"),
            }
        };

        Ok(SvcIncomingCall(svc, call_request))
    }

    pub fn clear(self, cause_code: u8, diagnostic_code: u8) -> io::Result<()> {
        let inner = self.0;

        {
            // Send the clear request packet.
            {
                let mut state = inner.state.0.lock().unwrap();

                if !state.is_connected() {
                    // TODO: what about if the state was cleared by the peer?
                    // is that an error... we don't get to clear with OUR cause_code...
                    todo!("invalid state");
                }

                inner.clear_request(
                    &mut state,
                    cause_code,
                    diagnostic_code,
                    ClearInitiator::Local,
                );
                inner.engine_wait.notify_all();
            }

            // Wait for the result.
            let mut state = inner.state.0.lock().unwrap();

            while matches!(*state, VcState::WaitClearConfirm(_, _)) {
                state = inner.state.1.wait(state).unwrap();
            }

            match *state {
                VcState::Cleared(ClearInitiator::Local, _) => { /* This is the expected state */ }
                VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
                _ => panic!("unexpected state"),
            }
        }

        // Even if the client cleared, there may be another thread waiting on
        // recv...
        //
        // TODO: should we move this to "cleared", the function that changes
        // the state?
        inner.recv_data_queue.1.notify_all();

        // TODO: It would be nice to be able to return the XotLink to the caller,
        // but that would require shutting down the receiver thread so that we
        // can take sole ownership of the link...
        //
        // Alternatavely, it may make sense to move the thread into the XotLink
        // so that we can simply return that to the caller.
        //
        // For now we'll just close the socket here, it's not obvious that it even
        // makes sense in the case of an XOT link to reuse it for another call.
        let _ = inner.send_link.lock().unwrap().shutdown();

        Ok(())
    }

    pub fn cleared(&self) -> Option<(u8, u8)> {
        let state = self.0.state.0.lock().unwrap();

        match *state {
            VcState::Cleared(ClearInitiator::Remote(ref clear_request), _) => {
                Some((clear_request.cause_code, clear_request.diagnostic_code))
            }
            _ => None,
        }
    }

    fn new(link: XotLink, channel: u16, params: &X25Params) -> Self {
        let (send_link, recv_link) = split_xot_link(link);

        let inner = Arc::new(VcInner::new(send_link, channel, params));

        let barrier = Arc::new(Barrier::new(2));

        thread::Builder::new()
            .name("x25_vc_1".to_string())
            .spawn({
                let inner = Arc::clone(&inner);
                let barrier = Arc::clone(&barrier);

                move || inner.run(recv_link, &barrier)
            })
            .expect("failed to spawn thread");

        barrier.wait();

        Svc(inner)
    }
}

/// Incoming X.25 _call_ that can be accepted, or cleared.
pub struct SvcIncomingCall(Svc, X25CallRequest);

impl SvcIncomingCall {
    pub fn request(&self) -> &X25CallRequest {
        &self.1
    }

    pub fn accept(self) -> io::Result<Svc> {
        let svc = self.0;

        {
            let inner = &svc.0;

            let mut state = inner.state.0.lock().unwrap();

            if !matches!(*state, VcState::Called(_)) {
                return Err(io::Error::other(
                    "other party probably gave up, or link is now out of order",
                ));
            }

            let call_accept = create_call_accept(inner.channel, &inner.params.read().unwrap());

            if let Err(err) = inner.send_packet(&call_accept.into()) {
                inner.out_of_order(&mut state, err);
                inner.engine_wait.notify_all();

                return Err(io::Error::other("link is out of order"));
            }

            inner.data_transfer(&mut state);
            inner.engine_wait.notify_all();
        }

        Ok(svc)
    }

    pub fn clear(self, cause_code: u8, diagnostic_code: u8) -> io::Result<()> {
        let inner = self.0 .0;

        let mut state = inner.state.0.lock().unwrap();

        if !matches!(*state, VcState::Called(_)) {
            return Err(io::Error::other(
                "other party probably gave up, or link is now out of order",
            ));
        }

        let clear_request = X25ClearRequest {
            modulo: inner.params.read().unwrap().modulo,
            channel: inner.channel,
            cause_code,
            diagnostic_code,
            called_addr: X121Addr::null(),
            calling_addr: X121Addr::null(),
            facilities: Vec::new(),
            clear_user_data: Bytes::new(),
        };

        if let Err(err) = inner.send_packet(&clear_request.into()) {
            inner.out_of_order(&mut state, err);
            inner.engine_wait.notify_all();

            return Err(io::Error::other("link is out of order"));
        }

        inner.cleared(&mut state, ClearInitiator::Local, None);
        inner.engine_wait.notify_all();

        Ok(())
    }
}

impl Vc for Svc {
    fn send(&self, user_data: Bytes, qualifier: bool) -> io::Result<()> {
        let inner = &self.0;

        if !self.is_connected() {
            todo!("invalid state");
        }

        let packet_size = inner.params.read().unwrap().send_packet_size;

        {
            let mut queue = inner.send_data_queue.0.lock().unwrap();

            let mut packets = user_data.chunks(packet_size).peekable();

            while let Some(packet) = packets.next() {
                let is_last = packets.peek().is_none();

                queue.push_back(SendData {
                    user_data: Bytes::copy_from_slice(packet),
                    qualifier,
                    more: !is_last,
                });
            }
        }

        // TODO: should we send here? or just wake up the engine and let it try?
        {
            let mut state = inner.state.0.lock().unwrap();

            inner.send_queued_data(&mut state);

            // TODO: check the state (could be out of order now) and alert the
            // client, probably...
        }

        Ok(())
    }

    fn recv(&self) -> io::Result<Option<(Bytes, bool)>> {
        let inner = &self.0;

        // TODO: introduce another outer "recv" lock, maybe, but for now...

        loop {
            // NOTE: state and recv_data_queue lock acquisition order is important
            // to avoid deadlock.
            let state = inner.state.0.lock().unwrap();

            let mut queue = inner.recv_data_queue.0.lock().unwrap();

            if let Some(data) = pop_complete_data(&mut queue) {
                return Ok(Some(data));
            }

            if !state.is_connected() {
                match *state {
                    VcState::Cleared(ClearInitiator::Local | ClearInitiator::Remote(_), _) => {
                        return Ok(None);
                    }
                    VcState::Cleared(ClearInitiator::TimeOut(_), _) => {
                        return Err(io::Error::from(io::ErrorKind::TimedOut));
                    }
                    VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
                    _ => panic!("unexpected state"),
                }
            }

            drop(state);

            // drop the lock on the queue, we'll reaquire above to maintain
            // acquisition order
            drop(inner.recv_data_queue.1.wait(queue).unwrap());
        }
    }

    fn reset(&self, cause_code: u8, diagnostic_code: u8) -> io::Result<()> {
        let inner = &self.0;

        // Send the reset request packet.
        {
            let mut state = inner.state.0.lock().unwrap();

            if !matches!(*state, VcState::DataTransfer(_)) {
                // TODO: what states is this valid in?
                todo!("invalid state");
            }

            inner.reset_request(&mut state, cause_code, diagnostic_code);
            inner.engine_wait.notify_all();
        }

        // Wait for the result.
        let mut state = inner.state.0.lock().unwrap();

        while matches!(*state, VcState::WaitResetConfirm(_)) {
            state = inner.state.1.wait(state).unwrap();
        }

        match *state {
            VcState::DataTransfer(_) => { /* This is the expected state */ }
            VcState::WaitClearConfirm(_, ClearInitiator::TimeOut(_))
            | VcState::Cleared(ClearInitiator::TimeOut(_), _) => {
                return Err(io::Error::from(io::ErrorKind::TimedOut))
            }
            VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
            _ => panic!("unexpected state"),
        };

        Ok(())
    }

    fn flush(&self) -> io::Result<()> {
        let inner = &self.0;

        loop {
            // NOTE: state and send_data_queue lock acquisition order is important
            // to avoid deadlock.
            let state = inner.state.0.lock().unwrap();

            let queue = inner.send_data_queue.0.lock().unwrap();

            if queue.is_empty() {
                return Ok(());
            }

            if !state.is_connected() {
                match *state {
                    VcState::Cleared(ClearInitiator::Local | ClearInitiator::Remote(_), _) => {
                        todo!("what error is this?");
                    }
                    VcState::Cleared(ClearInitiator::TimeOut(_), _) => {
                        return Err(io::Error::from(io::ErrorKind::TimedOut));
                    }
                    VcState::OutOfOrder => return Err(io::Error::other("link is out of order")),
                    _ => panic!("unexpected state"),
                }
            }

            drop(state);

            // drop the lock on the queue, we'll reaquire above to maintain
            // acquisition order
            drop(inner.send_data_queue.1.wait(queue).unwrap());
        }
    }

    fn params(&self) -> X25Params {
        self.0.params.read().unwrap().clone()
    }

    fn is_connected(&self) -> bool {
        let state = self.0.state.0.lock().unwrap();

        state.is_connected()
    }
}

impl Clone for Svc {
    fn clone(&self) -> Self {
        // TODO: is an appropriate way to do this, it may be better to "split" into a read
        // and write half.
        Svc(Arc::clone(&self.0))
    }
}

struct VcInner {
    send_link: Arc<Mutex<XotLink>>,
    engine_wait: Arc<Condvar>,
    channel: u16,
    state: Arc<(Mutex<VcState>, Condvar)>,
    params: Arc<RwLock<X25Params>>,
    send_data_queue: Arc<(Mutex<VecDeque<SendData>>, Condvar)>,
    recv_data_queue: Arc<(Mutex<VecDeque<X25Data>>, Condvar)>,
}

struct SendData {
    user_data: Bytes,
    qualifier: bool,
    more: bool,
}

impl VcInner {
    fn new(send_link: XotLink, channel: u16, params: &X25Params) -> Self {
        let state = VcState::Ready;

        VcInner {
            send_link: Arc::new(Mutex::new(send_link)),
            engine_wait: Arc::new(Condvar::new()),
            channel,
            state: Arc::new((Mutex::new(state), Condvar::new())),
            params: Arc::new(RwLock::new(params.clone())),
            send_data_queue: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
            recv_data_queue: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
        }
    }

    fn run(&self, mut recv_link: XotLink, barrier: &Arc<Barrier>) {
        // Create another thread that reads packets, this allows the main loop
        // wait to be interrupted while the XOT socket read is blocked.
        let recv_queue = Arc::new(Mutex::new(VecDeque::<io::Result<Bytes>>::new()));

        thread::Builder::new()
            .name("x25_vc_2".to_string())
            .spawn({
                let recv_queue = Arc::clone(&recv_queue);
                let engine_wait = Arc::clone(&self.engine_wait);

                move || loop {
                    let packet = recv_link.recv();

                    let is_err = packet.is_err();

                    recv_queue.lock().unwrap().push_back(packet);
                    engine_wait.notify_all();

                    if is_err {
                        break;
                    }
                }
            })
            .expect("failed to spawn thread");

        barrier.wait();

        let mut recv_queue = recv_queue.lock().unwrap();

        loop {
            let mut timeout = Duration::from_secs(100_000); // TODO

            let packet = recv_queue.pop_front();

            // Handle a XOT link error, otherwise pass along the packet.
            let packet = match packet.transpose() {
                Ok(packet) => packet,
                Err(err) => {
                    let mut state = self.state.0.lock().unwrap();

                    self.out_of_order(&mut state, err);
                    self.recv_data_queue.1.notify_all();
                    break;
                }
            };

            // Decode the packet.
            let temp_packet = packet.clone(); // TODO: temporary logging
            let packet = match packet.map(X25Packet::decode).transpose() {
                Ok(packet) => packet,
                Err(err) => {
                    dbg!(err);
                    dbg!(temp_packet);
                    todo!("handle packet decode error");
                }
            };

            // Validate the packet.
            if let Some(ref _packet) = packet {
                // TODO...
            }

            // Handle the packet.
            {
                let mut state = self.state.0.lock().unwrap();

                self.handle_in_packet(packet, &mut state, &mut timeout);

                // Exit loop if we are in a terminal state.
                if matches!(*state, VcState::Cleared(_, _) | VcState::OutOfOrder) {
                    break;
                }
            }

            // Only wait if the queue is empty, otherwise don't wait as we won't
            // receive a wakeup call.
            if recv_queue.is_empty() {
                (recv_queue, _) = self.engine_wait.wait_timeout(recv_queue, timeout).unwrap();
            }
        }
    }

    fn handle_in_packet(
        &self,
        packet: Option<X25Packet>,
        state: &mut VcState,
        timeout: &mut Duration,
    ) {
        match *state {
            VcState::Ready => {
                if let Some(X25Packet::CallRequest(call_request)) = packet {
                    let mut params = self.params.write().unwrap();

                    *params = negotiate_called_params(&call_request, &params);

                    self.change_state(state, VcState::Called(call_request));
                }
            }
            VcState::Called(_) => {
                match packet {
                    Some(X25Packet::ClearRequest(clear_request)) => {
                        self.cleared(state, ClearInitiator::Remote(clear_request), None);
                    }
                    _ => { /* TODO: ignore? */ }
                }
            }
            VcState::WaitCallAccept(start_time) => {
                let elapsed = start_time.elapsed();
                let X25Params { t21, t23, .. } = *self.params.read().unwrap();

                *timeout = t21; // TODO: <- backup

                match packet {
                    Some(X25Packet::CallAccept(call_accept)) => {
                        {
                            let mut params = self.params.write().unwrap();

                            *params = negotiate_calling_params(&call_accept, &params);
                        }

                        self.data_transfer(state);
                    }
                    Some(X25Packet::ClearRequest(clear_request)) => {
                        self.cleared(state, ClearInitiator::Remote(clear_request), None);
                    }
                    Some(_) => { /* TODO: Ignore? */ }
                    None if elapsed > t21 => {
                        println!("T21 timeout, sending clear request...");

                        self.clear_request(
                            state,
                            19, // Local procedure error
                            49, // Time expired for incoming call
                            ClearInitiator::TimeOut(21),
                        );

                        *timeout = t23;
                    }
                    None => *timeout = t21 - elapsed,
                }
            }
            VcState::DataTransfer(ref mut data_transfer_state) => {
                match packet {
                    Some(X25Packet::Data(data)) => 'packet: {
                        if !data_transfer_state.update_recv_seq(data.send_seq) {
                            self.reset_request(
                                state, 5, // Local procedure error
                                1, // Invalid send sequence
                            );

                            break 'packet;
                        }

                        if !data_transfer_state.update_send_window(data.recv_seq) {
                            self.reset_request(
                                state, 5, // Local procedure error
                                2, // Invalid receive sequence
                            );

                            break 'packet;
                        }

                        // TODO: check for validation errors...
                        self.queue_recv_data(data);

                        let (sent_count, _) = self.send_queued_data(state);

                        if !matches!(*state, VcState::DataTransfer(_)) {
                            break 'packet;
                        }

                        // TODO: clean all of this up and work out if
                        // sometimes, we should hold off...
                        let is_local_ready = true;

                        if sent_count == 0 && is_local_ready {
                            self.receive_ready(state);
                        }
                    }
                    Some(X25Packet::ReceiveReady(receive_ready)) => 'packet: {
                        if !data_transfer_state.update_send_window(receive_ready.recv_seq) {
                            self.reset_request(
                                state, 5, // Local procedure error
                                2, // Invalid receive sequence
                            );

                            break 'packet;
                        }

                        self.send_queued_data(state);
                    }
                    Some(X25Packet::ResetRequest(_)) => {
                        self.reset_confirm(state);
                    }
                    Some(X25Packet::ClearRequest(clear_request)) => {
                        self.clear_confirm(state, clear_request);
                        self.recv_data_queue.1.notify_all();
                    }
                    Some(_) => { /* TODO: Ignore? */ }
                    None => {}
                }
            }
            VcState::WaitResetConfirm(start_time) => {
                let elapsed = start_time.elapsed();
                let X25Params { t22, t23, .. } = *self.params.read().unwrap();

                *timeout = t22; // TODO: backup!

                match packet {
                    Some(X25Packet::ResetConfirm(_)) => {
                        self.data_transfer(state);
                    }
                    Some(X25Packet::ResetRequest(_)) => {
                        self.reset_confirm(state);
                    }
                    Some(X25Packet::ClearRequest(clear_request)) => {
                        self.clear_confirm(state, clear_request);
                        self.recv_data_queue.1.notify_all();
                    }
                    None if elapsed > t22 => {
                        println!("T22 timeout, sending clear request...");

                        self.clear_request(
                            state,
                            19, // Local procedure error
                            51, // Time expired for reset request
                            ClearInitiator::TimeOut(22),
                        );

                        *timeout = t23;
                    }
                    None => *timeout = t22 - elapsed,
                    Some(_) => { /* TODO: Ignore? Or, do you think I need to send a reset request again!!! */
                    }
                }
            }
            VcState::WaitClearConfirm(start_time, ref initiator) => {
                let elapsed = start_time.elapsed();
                let t23 = self.params.read().unwrap().t23;

                *timeout = t23;

                match packet {
                    Some(X25Packet::ClearConfirm(clear_confirm)) => {
                        let initiator = initiator.clone();

                        self.cleared(state, initiator, Some(clear_confirm));
                    }
                    Some(X25Packet::ClearRequest(_)) => todo!(),
                    Some(_) => { /* TODO: Ignore? */ }
                    None if elapsed > t23 => {
                        println!("T23 timeout");

                        // TODO:
                        // For a timeout on a "call request timeout" that leads to a clear
                        // request Cisco sends "time expired for clear indication" twice (2
                        // retries of THIS state).
                        //
                        // what does it do for a user initiated clear?
                        let err = io::Error::from(io::ErrorKind::TimedOut);

                        self.out_of_order(state, err);
                    }
                    None => *timeout = t23 - elapsed,
                }
            }
            VcState::Cleared(_, _) | VcState::OutOfOrder => {
                // Ignore packet, we'll exit the loop below.
            }
        }
    }

    fn data_transfer(&self, state: &mut VcState) {
        let X25Params {
            modulo,
            send_window_size,
            ..
        } = *self.params.read().unwrap();

        let next_state = VcState::DataTransfer(DataTransferState {
            modulo,
            send_window: Window::new(send_window_size, modulo),
            recv_seq: 0,
        });

        self.change_state(state, next_state);
    }

    fn cleared(
        &self,
        state: &mut VcState,
        initiator: ClearInitiator,
        clear_confirm: Option<X25ClearConfirm>,
    ) {
        let next_state = VcState::Cleared(initiator, clear_confirm);

        self.change_state(state, next_state);
    }

    fn out_of_order(&self, state: &mut VcState, _err: io::Error) {
        let next_state = VcState::OutOfOrder;

        self.change_state(state, next_state);
    }

    fn clear_request(
        &self,
        state: &mut VcState,
        cause_code: u8,
        diagnostic_code: u8,
        initiator: ClearInitiator,
    ) {
        let clear_request = X25ClearRequest {
            modulo: self.params.read().unwrap().modulo,
            channel: self.channel,
            cause_code,
            diagnostic_code,
            called_addr: X121Addr::null(),
            calling_addr: X121Addr::null(),
            facilities: Vec::new(),
            clear_user_data: Bytes::new(),
        };

        if let Err(err) = self.send_packet(&clear_request.into()) {
            self.out_of_order(state, err);
        } else {
            let next_state = VcState::WaitClearConfirm(Instant::now(), initiator);

            self.change_state(state, next_state);
        }
    }

    fn clear_confirm(&self, state: &mut VcState, clear_request: X25ClearRequest) {
        let clear_confirm = X25ClearConfirm {
            modulo: self.params.read().unwrap().modulo,
            channel: self.channel,
            called_addr: X121Addr::null(),
            calling_addr: X121Addr::null(),
            facilities: Vec::new(),
        };

        if let Err(err) = self.send_packet(&clear_confirm.into()) {
            self.out_of_order(state, err);
        } else {
            self.cleared(state, ClearInitiator::Remote(clear_request), None);
        }
    }

    fn reset_request(&self, state: &mut VcState, cause_code: u8, diagnostic_code: u8) {
        let reset_request = X25ResetRequest {
            modulo: self.params.read().unwrap().modulo,
            channel: self.channel,
            cause_code,
            diagnostic_code,
        };

        if let Err(err) = self.send_packet(&reset_request.into()) {
            self.out_of_order(state, err);
        } else {
            let next_state = VcState::WaitResetConfirm(Instant::now());

            self.change_state(state, next_state);
        }
    }

    fn reset_confirm(&self, state: &mut VcState) {
        let reset_confirm = X25ResetConfirm {
            modulo: self.params.read().unwrap().modulo,
            channel: self.channel,
        };

        if let Err(err) = self.send_packet(&reset_confirm.into()) {
            self.out_of_order(state, err);
        } else {
            self.data_transfer(state);
        }
    }

    fn send_queued_data(&self, state: &mut VcState) -> (usize, usize) {
        let VcState::DataTransfer(ref mut data_transfer_state) = *state else {
            panic!("unexpected state")
        };

        let mut queue = self.send_data_queue.0.lock().unwrap();

        let mut count = 0;

        while !queue.is_empty() && data_transfer_state.send_window.is_open() {
            let SendData {
                user_data,
                qualifier,
                more,
            } = queue.front().unwrap();

            let data = X25Data {
                modulo: self.params.read().unwrap().modulo,
                channel: self.channel,
                send_seq: data_transfer_state.send_window.seq(),
                recv_seq: data_transfer_state.recv_seq,
                qualifier: *qualifier,
                delivery: false,
                more: *more,
                user_data: user_data.clone(),
            };

            if let Err(err) = self.send_packet(&data.into()) {
                self.out_of_order(state, err);
                break;
            }

            queue.pop_front();
            data_transfer_state.send_window.incr();

            count += 1;
        }

        if count > 0 {
            self.send_data_queue.1.notify_all();
        }

        (count, queue.len())
    }

    fn receive_ready(&self, state: &mut VcState) {
        let recv_seq = match *state {
            VcState::DataTransfer(ref data_transfer_state) => data_transfer_state.recv_seq,
            _ => panic!("unexpected state"),
        };

        let receive_ready = X25ReceiveReady {
            modulo: self.params.read().unwrap().modulo,
            channel: self.channel,
            recv_seq,
        };

        if let Err(err) = self.send_packet(&receive_ready.into()) {
            self.out_of_order(state, err);
        }
    }

    fn queue_recv_data(&self, data: X25Data) {
        let params = self.params.read().unwrap();

        if data.user_data.len() > params.recv_packet_size {
            // TODO: "Packet too long"
        }

        let mut queue = self.recv_data_queue.0.lock().unwrap();

        if let Some(prev_data) = queue.back() {
            if prev_data.more && data.qualifier != prev_data.qualifier {
                // TODO: "Inconsistent Q-bit setting"
            }
        }

        queue.push_back(data);
        self.recv_data_queue.1.notify_all();
    }

    fn change_state(&self, state: &mut VcState, new_state: VcState) {
        *state = new_state;
        self.state.1.notify_all();
    }

    fn send_packet(&self, packet: &X25Packet) -> io::Result<()> {
        let mut buf = BytesMut::new();

        packet.encode(&mut buf).map_err(io::Error::other)?;

        self.send_link.lock().unwrap().send(&buf)
    }
}

fn create_call_request(
    channel: u16,
    addr: &X121Addr,
    call_user_data: &[u8],
    params: &X25Params,
) -> X25CallRequest {
    let facilities = vec![
        X25Facility::PacketSize {
            from_called: params.recv_packet_size,
            from_calling: params.send_packet_size,
        },
        X25Facility::WindowSize {
            from_called: params.recv_window_size,
            from_calling: params.send_window_size,
        },
    ];

    X25CallRequest {
        modulo: params.modulo,
        channel,
        called_addr: addr.clone(),
        calling_addr: params.addr.clone(),
        facilities,
        call_user_data: Bytes::copy_from_slice(call_user_data),
    }
}

fn create_call_accept(channel: u16, params: &X25Params) -> X25CallAccept {
    let facilities = vec![
        X25Facility::PacketSize {
            from_called: params.send_packet_size,
            from_calling: params.recv_packet_size,
        },
        X25Facility::WindowSize {
            from_called: params.send_window_size,
            from_calling: params.recv_window_size,
        },
    ];

    X25CallAccept {
        modulo: params.modulo,
        channel,
        called_addr: X121Addr::null(),
        calling_addr: X121Addr::null(),
        facilities,
        called_user_data: Bytes::new(),
    }
}

fn negotiate_calling_params(call_accept: &X25CallAccept, params: &X25Params) -> X25Params {
    let mut params = params.clone();

    params.modulo = call_accept.modulo;

    // When negotiating facilities from a received call accept, we are the "calling"
    // party.
    let facilities = &call_accept.facilities;

    if let Some((from_called, from_calling)) = get_packet_size(facilities) {
        params.send_packet_size = from_calling;
        params.recv_packet_size = from_called;
    }

    if let Some((from_called, from_calling)) = get_window_size(facilities) {
        params.send_window_size = clamp_window_size(from_calling, params.modulo);
        params.recv_window_size = clamp_window_size(from_called, params.modulo);
    }

    params
}

fn negotiate_called_params(call_request: &X25CallRequest, params: &X25Params) -> X25Params {
    let mut params = params.clone();

    params.modulo = call_request.modulo;

    // When negotiating facilties from a received call request, we are the "called"
    // party.
    let facilities = &call_request.facilities;

    if let Some((from_called, from_calling)) = get_packet_size(facilities) {
        params.send_packet_size = from_called;
        params.recv_packet_size = from_calling;
    }

    if let Some((from_called, from_calling)) = get_window_size(facilities) {
        params.send_window_size = clamp_window_size(from_called, params.modulo);
        params.recv_window_size = clamp_window_size(from_calling, params.modulo);
    }

    params
}

fn get_packet_size(facilities: &[X25Facility]) -> Option<(usize, usize)> {
    facilities.iter().find_map(|f| match f {
        X25Facility::PacketSize {
            from_called,
            from_calling,
        } => Some((*from_called, *from_calling)),
        _ => None,
    })
}

fn get_window_size(facilities: &[X25Facility]) -> Option<(u8, u8)> {
    facilities.iter().find_map(|f| match f {
        X25Facility::WindowSize {
            from_called,
            from_calling,
        } => Some((*from_called, *from_calling)),
        _ => None,
    })
}

fn clamp_window_size(size: u8, modulo: X25Modulo) -> u8 {
    min(size, (modulo as u8) - 1)
}

impl DataTransferState {
    #[must_use]
    fn update_recv_seq(&mut self, seq: u8) -> bool {
        if seq != self.recv_seq {
            return false;
        }

        self.recv_seq = next_seq(seq, self.modulo);

        true
    }

    #[must_use]
    fn update_send_window(&mut self, seq: u8) -> bool {
        self.send_window.update_start(seq)
    }
}

fn pop_complete_data(queue: &mut VecDeque<X25Data>) -> Option<(Bytes, bool)> {
    if queue.is_empty() {
        return None;
    }

    let index = queue.iter().position(|d| !d.more)?;

    let packets: Vec<X25Data> = queue.drain(0..=index).collect();

    let user_data_len: usize = packets.iter().map(|p| p.user_data.len()).sum();

    let mut user_data = BytesMut::with_capacity(user_data_len);
    let mut qualifier = false;

    for packet in packets {
        user_data.put(packet.user_data);
        qualifier = packet.qualifier;
    }

    Some((user_data.freeze(), qualifier))
}

fn split_xot_link(link: XotLink) -> (XotLink, XotLink) {
    // crazy hack...
    let tcp_stream = link.into_stream();

    (
        XotLink::new(tcp_stream.try_clone().unwrap()),
        XotLink::new(tcp_stream),
    )
}
