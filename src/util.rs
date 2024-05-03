pub fn is_char_delete(byte: u8) -> bool {
    // Windows will generate BS...
    if cfg!(windows) && byte == /* BS */ 0x08 {
        return true;
    }

    // Others, probably, DEL...
    byte == /* DEL */ 0x7f
}
