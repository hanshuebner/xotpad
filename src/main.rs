use clap::Parser;
use libxotpad::pad::PadParams;
use libxotpad::x121::X121Addr;
use libxotpad::x25::{X25Modulo, X25Params};
use libxotpad::x3::{X3Echo, X3Editing, X3Forward, X3Idle, X3LfInsert};
use libxotpad::xot::{self, XotResolver};
use std::collections::HashMap;
use std::io;
use std::net::TcpListener;
use std::time::Duration;

use xotpad::user_pad;
use xotpad::x28::X28Selection;
use xotpad::x3::{UserPadParams, X3CharDelete, X3LineDelete, X3LineDisplay};

fn main() -> io::Result<()> {
    let args = Args::parse();

    let config = load_config(&args);

    if config.x25_params.addr.is_null() {
        eprintln!("warning: local address is null, use the --address option to specify an address");
    }

    let listener = if args.should_listen {
        if let Ok(listener) = TcpListener::bind((args.xot_bind_addr.as_str(), xot::TCP_PORT)) {
            Some(listener)
        } else {
            println!("unable to bind... will not listen!");
            None
        }
    } else {
        None
    };

    let svc = if let Some(selection) = &args.selection {
        match user_pad::call(selection, &config.x25_params, &config.resolver) {
            Ok(svc) => Some(svc),
            Err(err) => {
                return Err(io::Error::new(io::ErrorKind::Other, err));
            }
        }
    } else {
        None
    };

    user_pad::run(
        &config.x25_params,
        &config.x3_profiles,
        &config.resolver,
        config.x3_profile,
        listener,
        svc,
    )
}

// -c, --config <FILE>          config file
// -P, --x25-profile <PROFILE>  X.25 profile
// -s, --serve                  SERVE!

/// X.25 PAD for XOT.
#[derive(Parser, Debug)]
struct Args {
    /// Local X.121 address.
    #[arg(short = 'a', long = "address", value_name = "ADDRESS")]
    local_addr: Option<X121Addr>,

    /// XOT gateway.
    #[arg(short = 'g', long = "gateway", value_name = "GATEWAY")]
    xot_gateway: Option<String>,

    /// Bind address for incoming XOT connections.
    #[arg(
        short = 'b',
        long = "bind",
        default_value = "0.0.0.0",
        value_name = "ADDRESS"
    )]
    xot_bind_addr: String,

    /// X.3 profile.
    #[arg(
        short = 'p',
        long = "x3-profile",
        default_value = "default",
        value_name = "PROFILE"
    )]
    x3_profile: String,

    /// Listen for incoming calls.
    #[arg(short = 'l', long = "listen")]
    should_listen: bool,

    /// X.28 selection.
    #[arg(value_name = "SELECTION", conflicts_with = "should_listen")]
    selection: Option<X28Selection>,
}

struct Config<'a> {
    x25_params: X25Params,
    x3_profiles: HashMap<&'a str, PadParams<UserPadParams>>,
    resolver: XotResolver,
    x3_profile: &'a str,
}

fn load_config(args: &Args) -> Config {
    let addr = match args.local_addr {
        Some(ref local_addr) => local_addr.clone(),
        None => X121Addr::null(),
    };

    let x25_params = X25Params {
        addr,
        modulo: X25Modulo::Normal,
        send_packet_size: 128,
        send_window_size: 2,
        recv_packet_size: 128,
        recv_window_size: 2,
        t21: Duration::from_secs(5),
        t22: Duration::from_secs(5),
        t23: Duration::from_secs(5),
    };

    // TODO...
    let mut x3_profiles = HashMap::new();

    x3_profiles.insert(
        "default",
        PadParams {
            echo: X3Echo::try_from(1).unwrap(),
            forward: X3Forward::try_from(126).unwrap(),
            idle: X3Idle::from(0),
            lf_insert: X3LfInsert::try_from(0).unwrap(),
            editing: X3Editing::try_from(0).unwrap(),
            delegate: Some(UserPadParams {
                char_delete: X3CharDelete::try_from(127).unwrap(),
                line_delete: X3LineDelete::try_from(/* Ctrl+X */ 24).unwrap(),
                line_display: X3LineDisplay::try_from(/* Ctrl+R */ 18).unwrap(),
            }),
        },
    );

    let mut resolver = XotResolver::new();

    if let Some(ref xot_gateway) = args.xot_gateway {
        let _ = resolver.add(".*", xot_gateway);
    } else {
        #[cfg(feature = "x25_org")]
        let _ = resolver.add("^(...)(...)", "\\2.\\1.x25.org");
    }

    // TODO...
    let x3_profile = args.x3_profile.as_str();

    if !x3_profiles.contains_key(x3_profile) {
        panic!("uuuh that X.3 profile does not exist!");
    }

    Config {
        x25_params,
        x3_profiles,
        resolver,
        x3_profile,
    }
}
