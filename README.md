# xotpad

X.25 PAD for XOT.

## Features

Okay, so this is just a prototype for now... the goal is to create a cross-platform user space
[PAD](https://en.wikipedia.org/wiki/Packet_assembler/disassembler)
allowing access to X.25 networks using XOT described in
[RFC 1613](https://www.rfc-editor.org/rfc/rfc1613.html).

  - [x] User space X.25 over TCP (XOT)
      - [x] Modulo 8
      - [x] Modulo 128
      - [x] Flow control parameter negotiation (packet and window size)
  - [x] Interactive _Triple-X_ PAD (X.3, X.28 and X.29)
  - [ ] Host PAD providing access to local processes
  - [x] DNS-based resolution of...
      - [x] XOT gateways
      - [ ] X.121 addresses

## Usage

### Quick Start

To connect to X.25 host 737411:

```
xotpad 737411
```

By default, _xotpad_ will use the DNS-based X.25 routing service provided by
[x25.org](https://x25.org/) to lookup the XOT gateway for a X.121 address. To override this and
specify a XOT gateway, use the `-g` option:

```
xotpad -g my-cisco-router 123456
```

To start an interactive X.28 PAD:

<pre>
$ <b>xotpad</b>
*<b>call 737411</b>
...
<kbd><kbd>Ctrl</kbd>+<kbd>P</kbd></kbd>
*<b>exit</b>
$
</pre>

Use <kbd><kbd>Ctrl</kbd>+<kbd>P</kbd></kbd> to recall the PAD, this is similar to the _telnet_
<kbd><kbd>Ctrl</kbd>+<kbd>]</kbd></kbd> sequence.

To exit the interactive PAD, use the `exit` command.

By default, the interactive PAD will not accept incoming calls. To listen for, and accept,
incoming calls use the `-l` option:

```
xotpad -l
```

Incoming calls will be automatically accepted, assuming the PAD is free.
