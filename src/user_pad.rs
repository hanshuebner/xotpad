use bytes::{BufMut, Bytes, BytesMut};
use chrono::{DateTime, Local};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use libxotpad::pad::{Pad, PadParams};
use libxotpad::x25::packet::X25CallRequest;
use libxotpad::x25::{Svc, Vc, X25Params};
use libxotpad::x3::X3Params;
use libxotpad::xot::{self, XotLink, XotResolver};
use std::collections::HashMap;
use std::io::{self, BufReader, Read, Write};
use std::net::TcpListener;
use std::str::{self, FromStr};
use std::sync::mpsc::{channel, Sender};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;
use tracing_mutex::stdsync::{Mutex, RwLock};

use crate::util::is_char_delete;
use crate::x28::{X28Addr, X28Command, X28Selection, X28Signal};
use crate::x3::UserPadParams;

pub fn call(
    selection: &X28Selection,
    x25_params: &X25Params,
    resolver: &XotResolver,
) -> io::Result<Svc> {
    assert!(!selection.addrs.is_empty());

    if selection.addrs.len() > 1 {
        todo!("multiple addresses");
    }

    let addr = &selection.addrs[0];

    let X28Addr::Full(addr) = addr else {
        todo!("abbreviated addresses");
    };

    if !selection.facilities.is_empty() {
        todo!("facilities");
    }

    if !selection.call_user_data.is_empty() {
        todo!("call user data");
    }

    let xot_link = xot::connect(addr, resolver)?;

    let call_user_data = Bytes::from_static(b"\x01\x00\x00\x00");

    Svc::call(xot_link, 1, addr, &call_user_data, x25_params)
}

#[derive(Copy, Clone, PartialEq)]
enum PadLocalState {
    Command,
    Data,
}

enum PadInput {
    Call(X25CallRequest),
    Local(io::Result<Option<u8>>),
    Remote(io::Result<Option<Bytes>>),
}

pub fn run(
    x25_params: &X25Params,
    x3_profiles: &HashMap<&str, PadParams<UserPadParams>>,
    resolver: &XotResolver,
    x3_profile: &str,
    tcp_listener: Option<TcpListener>,
    svc: Option<Svc>,
) -> io::Result<()> {
    let (tx, rx) = channel();

    enable_raw_mode()?;

    // Start the local input thread.
    thread::Builder::new()
        .name("user_pad_local".to_string())
        .spawn({
            let tx = tx.clone();

            move || {
                let reader = BufReader::new(io::stdin());

                for byte in reader.bytes() {
                    let should_continue = byte.is_ok();

                    let input = byte.map(Some);

                    if tx.send(PadInput::Local(input)).is_err() {
                        break;
                    }

                    if !should_continue {
                        break;
                    }
                }

                let _ = tx.send(PadInput::Local(Ok(None)));
            }
        })
        .expect("failed to spawn thread");

    let mut local_state = PadLocalState::Command;
    let mut is_one_shot = false;

    let x3_params = Arc::new(RwLock::new(
        x3_profiles
            .get(x3_profile)
            .expect("unknown X.3 profile")
            .clone(),
    ));

    let current_call = Arc::new(Mutex::new(Option::<(Pad<UserPadParams>, X25Params)>::None));

    if let Some(svc) = svc {
        let x25_params = svc.params();

        let pad = Pad::new(svc, Arc::clone(&x3_params), true);

        current_call.lock().unwrap().replace((pad, x25_params));

        local_state = PadLocalState::Data;
        is_one_shot = true;

        {
            let current_call = current_call.lock().unwrap();

            let (pad, _) = current_call.as_ref().unwrap();

            spawn_remote_thread(pad, tx.clone());
        }
    }

    if let Some(tcp_listener) = tcp_listener {
        let x25_params = x25_params.clone();
        let x3_params = Arc::clone(&x3_params);
        let current_call = Arc::clone(&current_call);
        let tx = tx.clone();

        thread::Builder::new()
            .name("user_pad_listener".to_string())
            .spawn(move || {
                for tcp_stream in tcp_listener.incoming() {
                    if tcp_stream.is_err() {
                        continue;
                    }

                    let xot_link = XotLink::new(tcp_stream.unwrap());

                    let incoming_call = Svc::listen_timeout(
                        xot_link,
                        1, /* this "channel" needs to be removed! */
                        &x25_params,
                        Duration::from_secs(200),
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

                    let call_request = incoming_call.request().clone();

                    let svc = incoming_call.accept().unwrap();

                    let x25_params = svc.params();

                    // TODO: should we "reset" the X.3 parameters here, for a new
                    // call?

                    let pad = Pad::new(svc, Arc::clone(&x3_params), true);

                    current_call.replace((pad, x25_params));

                    if tx.send(PadInput::Call(call_request)).is_err() {
                        break;
                    }
                }
            })
            .expect("failed to spawn thread");
    }

    let mut command_buf = BytesMut::with_capacity(128);
    let mut line_buf = BytesMut::with_capacity(128);

    if local_state == PadLocalState::Command {
        print_prompt();

        io::stdout().flush()?;
    }

    'main: loop {
        let Ok(input) = rx.recv() else {
            break;
        };

        let mut current_call = current_call.lock().unwrap();

        match input {
            PadInput::Call(call_request) => {
                print_signal(X28Signal::Connected(Some(call_request)), true);

                local_state = PadLocalState::Data;

                let (pad, _) = current_call.as_ref().unwrap();

                spawn_remote_thread(pad, tx.clone());
            }
            PadInput::Remote(Ok(Some(buf))) => {
                io::stdout().write_all(&buf)?;
            }
            PadInput::Remote(Ok(None)) => {
                // If there is a current call then the clear was requested by the remote party,
                // it may have been as a result of an invite clear X.29 PAD message from us.
                let signal = if current_call.is_some() {
                    let (pad, _) = current_call.take().unwrap();

                    let cleared = pad.into_svc().cleared();

                    Some(X28Signal::Cleared(cleared))
                } else {
                    None
                };

                if is_one_shot {
                    break;
                }

                let mut new_line = true;

                if let Some(signal) = signal {
                    print_signal(signal, true);

                    new_line = false;
                }

                ensure_command(&mut local_state, new_line);
            }
            PadInput::Remote(Err(err)) => {
                println!("remote error: {err:?}");

                current_call.take();

                if is_one_shot {
                    break;
                }

                ensure_command(&mut local_state, true);
            }
            PadInput::Local(Ok(None) | Err(_)) => {
                if let Some((mut pad, _)) = current_call.take() {
                    pad.flush()?;
                    pad.into_svc().clear(0, 0)?; // TODO
                }
            }
            PadInput::Local(Ok(Some(byte))) => match (local_state, byte) {
                (PadLocalState::Command, /* Ctrl+C */ 0x03) => {
                    if command_buf.is_empty() {
                        if let Some((pad, _)) = current_call.take() {
                            pad.into_svc().clear(0, 0)?;
                        }

                        break;
                    }

                    command_buf.clear();

                    print!("\r\n");
                    print_prompt();
                }
                (PadLocalState::Command, /* Ctrl+P */ 0x10) => {
                    if command_buf.is_empty() && current_call.is_some() {
                        let (pad, _) = current_call.as_mut().unwrap();

                        pad.write_all(&[byte])?;

                        print!("\r\n");
                        local_state = PadLocalState::Data;
                    }
                }
                (PadLocalState::Command, byte) => 'input: {
                    let Some(line) = handle_command_input(&mut command_buf, byte)? else {
                        break 'input;
                    };

                    let line = line.trim();

                    if !line.is_empty() {
                        if line.to_uppercase() == "EXIT" {
                            if let Some((x29_pad, _)) = current_call.take() {
                                x29_pad.into_svc().clear(0, 0)?;
                            }

                            break 'main;
                        }

                        let command = X28Command::from_str(line);

                        match command {
                            Ok(X28Command::Selection(ref selection)) => {
                                if current_call.is_some() {
                                    print_signal(X28Signal::Error, false); // Connected
                                } else {
                                    match call(selection, x25_params, resolver) {
                                        Ok(svc) => {
                                            let x25_params = svc.params();

                                            let pad = Pad::new(svc, Arc::clone(&x3_params), true);

                                            current_call.replace((pad, x25_params));

                                            print_signal(X28Signal::Connected(None), false);

                                            local_state = PadLocalState::Data;

                                            let (pad, _) = current_call.as_ref().unwrap();

                                            spawn_remote_thread(pad, tx.clone());
                                        }
                                        Err(xxx) => print!("SOMETHING WENT WRONG: {xxx}\r\n"),
                                    }
                                }
                            }
                            Ok(X28Command::Clear) => {
                                if let Some((pad, _)) = current_call.take() {
                                    pad.into_svc().clear(0, 0)?;

                                    print_signal(X28Signal::Cleared(None), false);
                                } else {
                                    print_signal(X28Signal::Error, false); // Not connected
                                }

                                if is_one_shot {
                                    break 'main;
                                }
                            }
                            Ok(X28Command::Read(ref request)) => {
                                let response = read_params(&x3_params.read().unwrap(), request);

                                print_signal(X28Signal::LocalParams(response), false);
                            }
                            Ok(X28Command::Set(ref request)) => {
                                let response = set_params(&mut x3_params.write().unwrap(), request);

                                // Only invalid requests are output by the set command.
                                let invalid: Vec<(u8, Option<u8>)> =
                                    response.into_iter().filter(|(_, r)| r.is_none()).collect();

                                if !invalid.is_empty() {
                                    print_signal(X28Signal::LocalParams(invalid), false);
                                }
                            }
                            Ok(X28Command::SetRead(ref request)) => {
                                let response = set_params(&mut x3_params.write().unwrap(), request);

                                print_signal(X28Signal::LocalParams(response), false);
                            }
                            Ok(X28Command::Status) => {
                                let signal = if current_call.is_some() {
                                    X28Signal::Engaged
                                } else {
                                    X28Signal::Free
                                };

                                print_signal(signal, false);
                            }
                            Ok(X28Command::InviteClear) => {
                                if let Some((pad, _)) = current_call.as_ref() {
                                    pad.invite_clear()?;

                                    // TODO: Implement timeout, if clear request not received from
                                    // remote PAD send a clear request.
                                } else {
                                    print_signal(X28Signal::Error, false); // Not connected
                                }
                            }
                            Ok(X28Command::Help(subject)) => print_help(&subject),
                            Err(_) => print_signal(X28Signal::Error, false),
                        }
                    }

                    if current_call.is_some() {
                        local_state = PadLocalState::Data;
                    } else {
                        print_prompt();
                    }
                }
                (PadLocalState::Data, /* Ctrl+P */ 0x10) => {
                    ensure_command(&mut local_state, true);
                }
                (PadLocalState::Data, byte) => 'input: {
                    let (pad, _) = current_call.as_mut().unwrap();

                    let pad_params = x3_params.read().unwrap();
                    let params = pad_params.delegate.as_ref().unwrap();

                    let editing: bool = pad_params.editing.into();

                    if editing {
                        if params.char_delete.is_match(byte) {
                            handle_char_delete(&mut line_buf)?;
                            break 'input;
                        } else if params.line_delete.is_match(byte) {
                            handle_line_delete(&mut line_buf)?;
                            break 'input;
                        } else if params.line_display.is_match(byte) {
                            handle_line_display(&line_buf)?;
                            break 'input;
                        }

                        line_buf.put_u8(byte);

                        if pad_params.echo.into() {
                            io::stdout().write_all(&[byte])?;

                            if pad_params.lf_insert.after_echo(byte) {
                                io::stdout().write_all(&[/* LF */ 0x0a])?;
                            }
                        }

                        // Need to drop read lock on PAD parameters for lock ordering on write.
                        drop(pad_params);

                        if byte == /* CR */ 0x0d {
                            pad.write_all(&line_buf.split())?;
                        }
                    } else {
                        // Need to drop read lock on PAD parameters for lock ordering on write.
                        drop(pad_params);

                        pad.write_all(&[byte])?;
                    }
                }
            },
        }

        io::stdout().flush()?;
    }

    io::stdout().flush()?;

    disable_raw_mode()?;

    Ok(())
}

fn spawn_remote_thread(pad: &Pad<UserPadParams>, channel: Sender<PadInput>) -> JoinHandle<()> {
    let mut pad = pad.clone();

    thread::Builder::new()
        .name("user_pad_remote".to_string())
        .spawn(move || {
            let mut buf = [0; 128];

            loop {
                let (input, should_continue) = match pad.read(&mut buf[..]) {
                    Ok(0) => (PadInput::Remote(Ok(None)), false),
                    Ok(n) => {
                        let buf = Bytes::copy_from_slice(&buf[..n]);

                        (PadInput::Remote(Ok(Some(buf))), true)
                    }
                    Err(err) => (PadInput::Remote(Err(err)), false),
                };

                if channel.send(input).is_err() {
                    break;
                }

                if !should_continue {
                    break;
                }
            }
        })
        .expect("failed to spawn thread")
}

fn read_params<Q: X3Params>(params: &PadParams<Q>, request: &[u8]) -> Vec<(u8, Option<u8>)> {
    if request.is_empty() {
        return params.all().iter().map(|&(p, v)| (p, Some(v))).collect();
    }

    request.iter().map(|&p| (p, params.get(p))).collect()
}

fn set_params<Q: X3Params>(
    params: &mut PadParams<Q>,
    request: &[(u8, u8)],
) -> Vec<(u8, Option<u8>)> {
    request
        .iter()
        .map(|&(p, v)| {
            if params.set(p, v).is_err() {
                return (p, None);
            }

            (p, params.get(p))
        })
        .collect()
}

fn handle_char_delete(buf: &mut BytesMut) -> io::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }

    buf.truncate(buf.len() - 1);

    io::stdout().write_all(&[0x08, 0x20, 0x08])
}

fn handle_line_delete(buf: &mut BytesMut) -> io::Result<()> {
    if buf.is_empty() {
        return Ok(());
    }

    buf.clear();

    // This is the indication the line delete function has completed for printing terminals. Video
    // terminals should use a rpetition of the BS SP BS sequence to clear the line but it appears
    // the Cisco x28 command just displays the printing terminal indication.
    io::stdout().write_all(b"XXX\r\n")
}

fn handle_line_display(buf: &BytesMut) -> io::Result<()> {
    io::stdout().write_all(b"\r\n")?;
    io::stdout().write_all(buf)
}

fn print_prompt() {
    print!("*");
}

fn print_signal(signal: X28Signal, new_line: bool) {
    if new_line {
        print!("\r\n");
    }

    print!("{signal}\r\n");
}

fn ensure_command(state: &mut PadLocalState, new_line: bool) {
    if *state == PadLocalState::Command {
        return;
    }

    if new_line {
        print!("\r\n");
    }

    print_prompt();

    *state = PadLocalState::Command;
}

fn handle_command_input(buf: &mut BytesMut, byte: u8) -> io::Result<Option<String>> {
    if byte == /* CR */ 0x0d {
        print!("\r\n");

        let buf = buf.split();

        let line = str::from_utf8(&buf[..]).unwrap();

        return Ok(Some(line.to_string()));
    }

    if is_char_delete(byte) {
        handle_char_delete(buf)?;

        return Ok(None);
    }

    // Ignore any other control characters, with the exception of escape which we need to
    // store in order to detect and remove escape sequences.
    if byte.is_ascii_control() && byte != /* ESC */ 0x1b {
        return Ok(None);
    }

    buf.put_u8(byte);

    if trim_complete_escape_sequence(buf) {
        return Ok(None);
    }

    if !in_escape_sequence(buf) {
        io::stdout().write_all(&[byte])?;
    }

    Ok(None)
}

fn in_escape_sequence(buf: &BytesMut) -> bool {
    buf.iter().rev().any(|&b| b == /* ESC */ 0x1b)
}

fn trim_complete_escape_sequence(buf: &mut BytesMut) -> bool {
    let mut sequence_len = 0;

    if buf.len() >= 3 {
        if buf[buf.len() - 3] == 0x1b && buf[buf.len() - 2] != b'O' {
            sequence_len = 3;
        }
    } else if buf.len() >= 4 && buf[buf.len() - 4] == 0x1b {
        sequence_len = 4;
    }

    if sequence_len > 0 {
        buf.truncate(buf.len() - sequence_len);

        return true;
    }

    false
}

fn print_help(subject: &str) {
    print!("\r\n");

    let subject = subject.to_uppercase();

    if subject.is_empty() || subject == "HELP" {
        let now: DateTime<Local> = Local::now();

        let year = now.format("%Y");

        print!("xotpad - {}\r\n", crate::ABOUT);
        print!("\r\n");
        print!("For more information on X.25 networking in {year}, visit https://x25.org\r\n");
    } else {
        print!("No help for subject, try HELP for a description of the HELP command\r\n");
    }

    print!("\r\n");
}
