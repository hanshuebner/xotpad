use bytes::{BufMut, Bytes, BytesMut};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use std::io::{self, BufReader, Read, Write};
use std::net::TcpListener;
use std::str::{self, FromStr};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use tracing_mutex::stdsync::TracingMutex;

use crate::x121::X121Addr;
use crate::x25::{Svc, Vc, X25Params};
use crate::xot::{self, XotLink, XotResolver};

use self::x28::X28Command;
use self::x29::X29PadMessage;

pub mod x28;
pub mod x29;

pub fn call(
    addr: &X121Addr,
    x25_params: &X25Params,
    resolver: &XotResolver,
) -> Result<Svc, String> {
    let xot_link = xot::connect(addr, resolver)?;

    let call_user_data = Bytes::from_static(b"\x01\x00\x00\x00");

    let svc = match Svc::call(xot_link, 1, addr, &call_user_data, x25_params) {
        Ok(svc) => svc,
        Err(err) => return Err("something went wrong with the call".into()),
    };

    Ok(svc)
}

#[derive(Copy, Clone, PartialEq)]
enum PadUserState {
    Command,
    Data,
}

#[derive(Debug)]
enum PadInput {
    Call,
    Network(io::Result<Option<(Bytes, bool)>>),
    User(io::Result<Option<u8>>),
}

pub fn run(
    x25_params: &X25Params,
    resolver: &XotResolver,
    tcp_listener: Option<TcpListener>,
    svc: Option<Svc>,
) -> io::Result<()> {
    let (tx, rx) = channel();

    enable_raw_mode()?;

    // Start the user input thread.
    thread::spawn({
        let tx = tx.clone();

        move || {
            let reader = BufReader::new(io::stdin());

            for byte in reader.bytes() {
                let should_continue = byte.is_ok();

                if tx.send(PadInput::User(byte.map(Some))).is_err() {
                    break;
                }

                if !should_continue {
                    break;
                }
            }

            let _ = tx.send(PadInput::User(Ok(None)));

            println!("done with user input thread");
        }
    });

    let current_call = Arc::new(TracingMutex::new(Option::<(Svc, X25Params)>::None));

    let mut user_state = PadUserState::Command;
    let mut is_one_shot = false;

    if let Some(svc) = svc {
        let x25_params = svc.params();

        current_call.lock().unwrap().replace((svc, x25_params));

        user_state = PadUserState::Data;
        is_one_shot = true;

        {
            let current_call = current_call.lock().unwrap();

            let (svc, _) = current_call.as_ref().unwrap();

            spawn_network_thread(svc, tx.clone());
        }
    }

    if let Some(tcp_listener) = tcp_listener {
        let x25_params = x25_params.clone();
        let current_call = Arc::clone(&current_call);
        let tx = tx.clone();

        thread::spawn(move || {
            for tcp_stream in tcp_listener.incoming() {
                if tcp_stream.is_err() {
                    continue;
                }

                let xot_link = XotLink::new(tcp_stream.unwrap());

                let incoming_call = Svc::listen(
                    xot_link,
                    1, /* this "channel" needs to be removed! */
                    &x25_params,
                );

                if incoming_call.is_err() {
                    continue;
                }

                let incoming_call = incoming_call.unwrap();

                let mut current_call = current_call.lock().unwrap();

                if current_call.is_some() {
                    let _ = incoming_call.clear(1, 0); // Number busy
                    continue;
                }

                let svc = incoming_call.accept().unwrap();

                let x25_params = svc.params();

                current_call.replace((svc, x25_params));

                if tx.send(PadInput::Call).is_err() {
                    break;
                }
            }

            println!("done with listener thread");
        });
    }

    let mut command_buf = BytesMut::with_capacity(128);
    let mut data_buf = BytesMut::with_capacity(128);

    if user_state == PadUserState::Command {
        print!("*");
        io::stdout().flush()?;
    }

    for input in rx {
        let mut current_call = current_call.lock().unwrap();

        match input {
            PadInput::Call => {
                println!("\r\nyou got a call!\r\n");

                user_state = PadUserState::Data;

                let (svc, _) = current_call.as_ref().unwrap();

                spawn_network_thread(svc, tx.clone());
            }
            PadInput::Network(Ok(Some((buf, true)))) => {
                let message = X29PadMessage::decode(buf);

                match message {
                    Ok(X29PadMessage::ClearInvitation) => {
                        // TODO: we should attempt to send all that we have before
                        // clearing...

                        current_call.take().unwrap().0.clear(0, 0)?;

                        if is_one_shot {
                            break;
                        }

                        ensure_command(&mut user_state);
                    }
                    Err(err) => println!("unrecognized or invalid X.29 PAD message"),
                }
            }
            PadInput::Network(Ok(Some((buf, false)))) => {
                let mut out = io::stdout().lock();

                out.write_all(&buf)?;
                out.flush()?;
            }
            PadInput::Network(Ok(None)) => {
                // XXX: we can tell whether we should show anything or not, based
                // on whether the SVC still "exists" otherwise we would have shown
                // the important info before...
                if current_call.is_some() {
                    let (svc, _) = current_call.take().unwrap();

                    let (cause, diagnostic_code) = svc.cleared().unwrap_or((0, 0));

                    println!("CLR xxx C:{cause} D:{diagnostic_code}");
                }

                if is_one_shot {
                    break;
                }

                ensure_command(&mut user_state);
            }
            PadInput::Network(Err(err)) => {
                println!("network error: {err:?}");

                current_call.take();

                if is_one_shot {
                    break;
                }

                ensure_command(&mut user_state);
            }
            PadInput::User(Ok(None) | Err(_)) => {
                println!("here");

                if current_call.is_none() {
                    break;
                }

                println!("not really sure what to do here yet...");
                println!("we probably need to wait for all data to be sent...");
                println!("then shut down cleanly.");
            }
            PadInput::User(Ok(Some(byte))) => match (user_state, byte) {
                (PadUserState::Command, /* Enter */ 0x0d) => {
                    let buf = command_buf.split();

                    let line = str::from_utf8(&buf[..]).unwrap().trim();

                    print!("\r\n");

                    if !line.is_empty() {
                        let command = X28Command::from_str(line);

                        match command {
                            Ok(X28Command::Call(ref addr)) => {
                                if current_call.is_some() {
                                    print!("ERROR... ENGAGED!\r\n");
                                } else {
                                    match call(addr, x25_params, resolver) {
                                        Ok(svc) => {
                                            let x25_params = svc.params();

                                            current_call.replace((svc, x25_params));

                                            user_state = PadUserState::Data;

                                            let (svc, _) = current_call.as_ref().unwrap();

                                            spawn_network_thread(svc, tx.clone());
                                        }
                                        Err(xxx) => print!("SOMETHING WENT WRONG: {xxx}\r\n"),
                                    }
                                }
                            }
                            Ok(X28Command::Clear) => {
                                if current_call.is_some() {
                                    current_call.take().unwrap().0.clear(0, 0)?;
                                } else {
                                    print!("ERROR... NOT CONNECTED!\r\n");
                                }

                                if is_one_shot {
                                    break;
                                }
                            }
                            Ok(X28Command::Status) => {
                                if current_call.is_some() {
                                    print!("ENGAGED\r\n");
                                } else {
                                    print!("FREE\r\n");
                                }
                            }
                            Ok(X28Command::Exit) => {
                                if current_call.is_some() {
                                    current_call.take().unwrap().0.clear(0, 0)?;
                                }

                                break;
                            }
                            Err(err) => {
                                print!("{err}\r\n");
                            }
                        }
                    }

                    if current_call.is_some() {
                        user_state = PadUserState::Data;
                    } else {
                        print!("*");
                        io::stdout().flush()?;
                    }
                }
                (PadUserState::Command, /* Ctrl+C */ 0x03) => {
                    if command_buf.is_empty() {
                        if current_call.is_some() {
                            current_call.take().unwrap().0.clear(0, 0)?;
                        }

                        break;
                    }

                    command_buf.clear();
                }
                (PadUserState::Command, /* Ctrl+P */ 0x10) => {
                    if command_buf.is_empty() && current_call.is_some() {
                        let (svc, x25_params) = current_call.as_ref().unwrap();

                        queue_and_send_data_if_ready(svc, x25_params, &mut data_buf, 0x10)?;

                        print!("\r\n");
                        user_state = PadUserState::Data;
                    }
                }
                (PadUserState::Command, byte) => {
                    command_buf.put_u8(byte);

                    io::stdout().write_all(&[byte])?;
                }
                (PadUserState::Data, /* Ctrl+P */ 0x10) => {
                    ensure_command(&mut user_state);
                }
                (PadUserState::Data, byte) => {
                    let (svc, x25_params) = current_call.as_ref().unwrap();

                    queue_and_send_data_if_ready(svc, x25_params, &mut data_buf, byte)?;
                }
            },
        }

        io::stdout().flush()?;
    }

    io::stdout().flush()?;

    disable_raw_mode()?;

    Ok(())
}

fn queue_and_send_data_if_ready(
    svc: &Svc,
    x25_params: &X25Params,
    buf: &mut BytesMut,
    byte: u8,
) -> io::Result<()> {
    buf.put_u8(byte);

    if is_data_ready_to_send(buf, x25_params) {
        let user_data = buf.split();

        return svc.send(user_data.into(), false);
    }

    Ok(())
}

// TODO: add x3_params to determine when to send!
fn is_data_ready_to_send(buf: &BytesMut, x25_params: &X25Params) -> bool {
    if buf.is_empty() {
        return false;
    }

    if buf.len() == x25_params.send_packet_size {
        return true;
    }

    let last_byte = buf.last().unwrap();

    // ...

    true
}

fn ensure_command(state: &mut PadUserState) {
    if *state == PadUserState::Command {
        return;
    }

    print!("\r\n*");

    *state = PadUserState::Command;
}

fn spawn_network_thread(svc: &Svc, channel: Sender<PadInput>) -> JoinHandle<()> {
    let svc = svc.clone();

    thread::spawn(move || {
        loop {
            let result = svc.recv();

            let should_continue = matches!(result, Ok(Some(_)));

            if channel.send(PadInput::Network(result)).is_err() {
                break;
            }

            if !should_continue {
                break;
            }
        }

        println!("done with network input thread");
    })
}
