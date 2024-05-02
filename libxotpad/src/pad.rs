use bytes::{Bytes, BytesMut};
use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::ops::Add;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread;
use std::time::Duration;
use std::time::Instant;
use tracing_mutex::stdsync::{Condvar, Mutex, RwLock};

use crate::x25::{Svc, Vc};
use crate::x29::X29PadMessage;
use crate::x3::{X3Echo, X3Editing, X3Forward, X3Idle, X3LfInsert, X3ParamError, X3Params};

type SendQueue = (VecDeque<u8>, Option<Instant>);
type IndicateChannelMessage = Vec<(u8, u8)>;

pub struct Pad<Q: X3Params + Send + Sync + 'static> {
    svc: Svc,
    params: Arc<RwLock<PadParams<Q>>>,
    should_suppress_echo_when_editing: bool,

    send_queue: Arc<(Mutex<SendQueue>, Condvar)>,
    recv_queue: Arc<(Mutex<VecDeque<u8>>, Condvar)>,
    recv_end: Arc<AtomicBool>,

    indicate_channel: Arc<Mutex<Option<Sender<IndicateChannelMessage>>>>,
}

impl<Q: X3Params + Send + Sync + 'static> Pad<Q> {
    pub fn new(
        svc: Svc,
        params: Arc<RwLock<PadParams<Q>>>,
        should_suppress_echo_when_editing: bool,
    ) -> Self {
        let send_queue = Arc::new((Mutex::new((VecDeque::new(), None)), Condvar::new()));
        let recv_queue = Arc::new((Mutex::new(VecDeque::new()), Condvar::new()));
        let recv_end = Arc::new(AtomicBool::new(false));
        let indicate_channel = Arc::new(Mutex::new(None));

        thread::Builder::new()
            .name("pad".to_string())
            .spawn({
                let svc = svc.clone();
                let params = Arc::clone(&params);
                let send_queue = Arc::clone(&send_queue);
                let recv_queue = Arc::clone(&recv_queue);
                let recv_end = Arc::clone(&recv_end);

                move || {
                    let mut should_clear = false;

                    loop {
                        let result = svc.recv();

                        match result {
                            Ok(Some((data, true))) => {
                                let message = X29PadMessage::decode(data);

                                match message {
                                    Ok(X29PadMessage::Set(request)) => {
                                        // According to the specification, a response message is only sent
                                        // if there are errors. It is not clear to me how that can be
                                        // handled by the remote party - how do they know how to long to
                                        // wait for an error response versus no response (indicating
                                        // success)?
                                        if let Some(message) =
                                            set_params(&mut *params.write().unwrap(), &request)
                                        {
                                            if let Err(_err) = send_message(&svc, message) {
                                                todo!();
                                            }
                                        }
                                    }
                                    Ok(X29PadMessage::Read(request)) => {
                                        let message =
                                            read_params(&*params.read().unwrap(), &request);

                                        if let Err(_err) = send_message(&svc, message) {
                                            todo!();
                                        }
                                    }
                                    Ok(X29PadMessage::SetRead(request)) => {
                                        let message = set_read_params(
                                            &mut *params.write().unwrap(),
                                            &request,
                                        );

                                        if let Err(_err) = send_message(&svc, message) {
                                            todo!();
                                        }
                                    }
                                    Ok(X29PadMessage::Indicate(_response)) => todo!(),
                                    Ok(X29PadMessage::ClearInvitation) => {
                                        if let Err(_err) = send_queued_data(
                                            &svc,
                                            &mut send_queue.0.lock().unwrap(),
                                        ) {
                                            todo!();
                                        }

                                        if let Err(_err) = svc.flush() {
                                            todo!();
                                        }

                                        should_clear = true;
                                        break;
                                    }
                                    Err(_) => todo!(),
                                }
                            }
                            Ok(Some((data, false))) => {
                                {
                                    let params = params.read().unwrap();
                                    let mut queue = recv_queue.0.lock().unwrap();

                                    queue_recv_data(&mut queue, data, &params);
                                }

                                recv_queue.1.notify_all();
                            }
                            Ok(None) => {
                                // TODO: wake up the indicate waiter... actually maybe
                                // we just do that at the end of the loop
                                break;
                            }
                            Err(_err) => {
                                todo!("ERR");

                                // TODO: wake up the indicate waiter... actually maybe
                                // we just do that at the end of the loop
                                //break;
                            }
                        }
                    }

                    if should_clear {
                        if let Err(_err) = svc.clear(0, 0) {
                            todo!("ERR");
                        }
                    }

                    recv_end.store(true, Ordering::Relaxed);
                    recv_queue.1.notify_all();
                }
            })
            .expect("failed to spawn thread");

        thread::Builder::new()
            .name("pad_send_idle".to_string())
            .spawn({
                let svc = svc.clone();
                let send_queue = Arc::clone(&send_queue);

                move || {
                    let mut queue = send_queue.0.lock().unwrap();

                    loop {
                        // If the deadline has expired, send the queued data.
                        if let Some(deadline) = queue.1 {
                            #[allow(clippy::collapsible_if)]
                            if !queue.0.is_empty() && Instant::now() >= deadline {
                                if send_queued_data(&svc, &mut queue).is_err() {
                                    break;
                                }
                            }
                        }

                        let timeout = queue.1.map_or(Duration::from_secs(10), |d| {
                            d.saturating_duration_since(Instant::now())
                        });

                        (queue, _) = send_queue.1.wait_timeout(queue, timeout).unwrap();
                    }
                }
            })
            .expect("failed to spawn thread");

        Pad {
            svc,
            params,
            should_suppress_echo_when_editing,
            send_queue,
            recv_queue,
            recv_end,
            indicate_channel,
        }
    }

    pub fn get_remote_params(&self, _request: &[u8]) -> Vec<(u8, u8)> {
        todo!()
    }

    pub fn set_remote_params(&self, _request: &[(u8, u8)]) -> Vec<(u8, u8)> {
        todo!()
    }

    pub fn invite_clear(&self) -> io::Result<()> {
        send_message(&self.svc, X29PadMessage::ClearInvitation)
    }

    pub fn clear(self, cause_code: u8, diagnostic_code: u8) -> io::Result<()> {
        self.svc.clear(cause_code, diagnostic_code)
    }

    pub fn into_svc(self) -> Svc {
        self.svc
    }

    fn should_echo_write(&self, params: &PadParams<Q>) -> bool {
        let echo: bool = params.echo.into();

        if !echo {
            return false;
        }

        if params.editing.into() && self.should_suppress_echo_when_editing {
            return false;
        }

        true
    }
}

impl<Q: X3Params + Send + Sync + 'static> Clone for Pad<Q> {
    fn clone(&self) -> Self {
        // TODO: is this an appropriate way to do this, it may be better to "split" into a read and
        // write half...
        Pad {
            svc: self.svc.clone(),
            params: Arc::clone(&self.params),
            should_suppress_echo_when_editing: self.should_suppress_echo_when_editing,
            send_queue: Arc::clone(&self.send_queue),
            recv_queue: Arc::clone(&self.recv_queue),
            recv_end: Arc::clone(&self.recv_end),
            indicate_channel: Arc::clone(&self.indicate_channel),
        }
    }
}

impl<Q: X3Params + Send + Sync + 'static> Read for Pad<Q> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let mut queue = self.recv_queue.0.lock().unwrap();

        loop {
            let mut index = 0;

            while index < buf.len() {
                let Some(byte) = queue.pop_front() else {
                    break;
                };

                buf[index] = byte;

                index += 1;
            }

            if index > 0 {
                return Ok(index);
            }

            // TODO: Is this correct, we've locked the queue so I think it's
            // okay to check here for just the end.
            if self.recv_end.load(Ordering::Relaxed) {
                return Ok(0);
            }

            queue = self.recv_queue.1.wait(queue).unwrap();
        }
    }
}

impl<Q: X3Params + Send + Sync + 'static> Write for Pad<Q> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }

        // TODO: if svc is closed then return Ok(0) or error?

        let packet_size = self.svc.params().send_packet_size;

        let mut count = 0;
        let mut should_wake_up_send = false;
        let mut should_wake_up_recv = false;

        {
            let params = self.params.read().unwrap();

            let should_echo = self.should_echo_write(&params);

            let idle: Option<Duration> = params.idle.into();

            let send_deadline = idle.map(|d| Instant::now().add(d));

            let mut send_queue = self.send_queue.0.lock().unwrap();
            let mut recv_queue = self.recv_queue.0.lock().unwrap(); // because of echo...

            for &byte in buf {
                if should_echo {
                    recv_queue.push_back(byte);

                    if params.lf_insert.after_echo(byte) {
                        recv_queue.push_back(/* LF */ 0x0a);
                    }

                    should_wake_up_recv = true;
                }

                send_queue.0.push_back(byte);

                if params.lf_insert.after_send(byte) {
                    send_queue.0.push_back(/* LF */ 0x0a);
                }

                if params.forward.is_match(byte) || send_queue.0.len() >= packet_size {
                    send_queued_data(&self.svc, &mut send_queue)?;
                }

                count += 1;
            }

            if !send_queue.0.is_empty() {
                if let Some(deadline) = send_deadline {
                    send_queue.1.replace(deadline);

                    should_wake_up_send = true;
                }
            }
        }

        if should_wake_up_send {
            self.send_queue.1.notify_all();
        }

        if should_wake_up_recv {
            self.recv_queue.1.notify_all();
        }

        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut queue = self.send_queue.0.lock().unwrap();

        send_queued_data(&self.svc, &mut queue)?;

        self.svc.flush()
    }
}

fn queue_recv_data<Q: X3Params>(queue: &mut VecDeque<u8>, data: Bytes, params: &PadParams<Q>) {
    for byte in data {
        queue.push_back(byte);

        if params.lf_insert.after_recv(byte) {
            queue.push_back(/* LF */ 0x0a);
        }
    }
}

fn send_queued_data(svc: &Svc, queue: &mut SendQueue) -> io::Result<()> {
    let buf = Bytes::from_iter(queue.0.drain(..));

    queue.1.take();

    svc.send(buf, false)
}

fn send_message(svc: &Svc, message: X29PadMessage) -> io::Result<()> {
    svc.flush()?;

    let mut buf = BytesMut::new();

    message.encode(&mut buf);

    svc.send(buf.into(), true)
}

fn read_params<Q: X3Params>(params: &Q, request: &[u8]) -> X29PadMessage {
    let response = if request.is_empty() {
        params.all().to_vec()
    } else {
        request
            .iter()
            .map(|&p| (p, params.get(p).unwrap_or(0x81)))
            .collect()
    };

    X29PadMessage::Indicate(response)
}

fn set_params<Q: X3Params>(params: &mut Q, request: &[(u8, u8)]) -> Option<X29PadMessage> {
    if request.is_empty() {
        // TODO: how do we reset the params here?
        todo!();
    }

    let response: Vec<(u8, u8)> = request
        .iter()
        .map(|&(p, v)| (p, params.set(p, v)))
        .filter_map(|(p, r)| {
            // TODO: improve this, so we can return a correct error code!
            if r.is_err() {
                Some((p, 0x80))
            } else {
                None
            }
        })
        .collect();

    if response.is_empty() {
        return None;
    }

    Some(X29PadMessage::Indicate(response))
}

fn set_read_params<Q: X3Params>(params: &mut Q, request: &[(u8, u8)]) -> X29PadMessage {
    if request.is_empty() {
        // TODO: how do we reset the params here?
        todo!();
    }

    let response: Vec<(u8, u8)> = request
        .iter()
        .map(|&(p, v)| {
            // TODO: improve this, so we can return a correct error code!
            if params.set(p, v).is_err() {
                return (p, 0x80);
            }

            (p, params.get(p).unwrap_or(0x81))
        })
        .collect();

    X29PadMessage::Indicate(response)
}

const PARAMS: [u8; 5] = [2, 3, 4, 13, 15];

#[derive(Clone, Debug)]
pub struct PadParams<Q: X3Params> {
    pub echo: X3Echo,

    pub forward: X3Forward,

    pub idle: X3Idle,

    pub lf_insert: X3LfInsert,

    pub editing: X3Editing,

    pub delegate: Option<Q>,
}

impl<Q: X3Params> X3Params for PadParams<Q> {
    fn get(&self, param: u8) -> Option<u8> {
        match (param, &self.delegate) {
            (2, _) => Some(*self.echo),
            (3, _) => Some(*self.forward),
            (4, _) => Some(*self.idle),
            (13, _) => Some(*self.lf_insert),
            (15, _) => Some(*self.editing),
            (_, Some(ref delegate)) => delegate.get(param),
            (_, None) => None,
        }
    }

    fn set(&mut self, param: u8, value: u8) -> Result<(), X3ParamError> {
        match (param, &mut self.delegate) {
            (2, _) => self.echo = X3Echo::try_from(value)?,
            (3, _) => self.forward = X3Forward::try_from(value)?,
            (4, _) => self.idle = X3Idle::from(value),
            (13, _) => self.lf_insert = X3LfInsert::try_from(value)?,
            (15, _) => self.editing = X3Editing::try_from(value)?,
            (_, Some(ref mut delegate)) => delegate.set(param, value)?,
            (_, None) => return Err(X3ParamError::Unsupported),
        };

        Ok(())
    }

    fn all(&self) -> Vec<(u8, u8)> {
        let mut params = Vec::new();

        for param in PARAMS {
            params.push((param, self.get(param).unwrap()));
        }

        // TODO: this does not reflect the precidence, we shouldn't
        // add any that already exist...
        if let Some(delegate) = &self.delegate {
            params.extend(delegate.all());
        }

        params
    }
}
