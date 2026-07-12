/// Removes ANSI CSI/OSC escape sequences and non-printing terminal controls.
/// Newlines and tabs are preserved; carriage returns are escaped to prevent line rewriting.
pub fn neutralize_ansi(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut output = String::with_capacity(input.len());
    let mut index = 0;
    let mut text_start = 0;

    while index < bytes.len() {
        if bytes[index] == 0x1b {
            output.push_str(&input[text_start..index]);
            index = consume_escape(bytes, index);
            text_start = index;
            continue;
        }
        if bytes[index] < 0x20 || bytes[index] == 0x7f {
            output.push_str(&input[text_start..index]);
            match bytes[index] {
                b'\n' => output.push('\n'),
                b'\t' => output.push('\t'),
                b'\r' => output.push_str("\\r"),
                _ => {}
            }
            index += 1;
            text_start = index;
            continue;
        }
        // Non-ASCII UTF-8 continuation bytes are copied as part of the surrounding slice.
        index += 1;
    }
    output.push_str(&input[text_start..]);
    output
}

fn consume_escape(bytes: &[u8], start: usize) -> usize {
    let Some(next) = bytes.get(start + 1).copied() else {
        return bytes.len();
    };
    match next {
        b'[' => {
            // Control Sequence Introducer: final byte is in 0x40..=0x7e.
            let mut index = start + 2;
            while index < bytes.len() {
                let byte = bytes[index];
                index += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
            index
        }
        b']' => {
            // Operating System Command: terminates with BEL or ST (ESC backslash).
            let mut index = start + 2;
            while index < bytes.len() {
                if bytes[index] == 0x07 {
                    return index + 1;
                }
                if bytes[index] == 0x1b && bytes.get(index + 1) == Some(&b'\\') {
                    return index + 2;
                }
                index += 1;
            }
            index
        }
        _ => (start + 2).min(bytes.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_color_and_osc_links_without_damaging_unicode() {
        let input =
            "\u{1b}[31merro\u{1b}[0m café \u{1b}]8;;https://evil.invalid\u{7}link\u{1b}]8;;\u{7}";
        assert_eq!(neutralize_ansi(input), "erro café link");
    }

    #[test]
    fn carriage_return_cannot_overwrite_prior_log_content() {
        assert_eq!(neutralize_ansi("safe\rsecret"), "safe\\rsecret");
    }
}
