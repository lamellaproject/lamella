//! The serve loop: read framed DAP requests, dispatch them to a [`Debugger`], and
//! write the responses and events back.

use crate::adapter::Debugger;
use crate::protocol::{Message, read_message, write_message};
use std::io::{self, BufRead, Write};

/// Reads requests from `reader`, dispatches each to `debugger`, and writes the
/// resulting responses and events to `writer`, until a `disconnect` request or
/// the end of the stream.
///
/// # Errors
/// Returns an [`io::Error`] if reading a frame, parsing it, or writing a reply
/// fails.
pub fn serve<R: BufRead, W: Write>(
    debugger: &mut Debugger,
    reader: &mut R,
    writer: &mut W,
) -> io::Result<()> {
    while let Some(message) = read_message(reader)? {
        let Message::Request(request) = message else {
            continue;
        };
        let disconnecting = request.command == "disconnect";
        for reply in debugger.handle(&request) {
            write_message(writer, &reply)?;
        }
        if disconnecting {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{Message, Request};
    use lamella_cil::{Instruction, MethodBodyImage, Opcode, Operand};
    use lamella_token::Token;
    use lamella_ves::Module;
    use std::io::Cursor;

    fn program() -> (Module, u32) {
        let mut module = Module::new();
        let write_line = module.add_intrinsic(lamella_ves::intrinsics::console_write_line, 1);
        module.bind_token(Token(0x0A00_0001), write_line);
        let hi: Vec<u16> = "hi".encode_utf16().collect();
        module.bind_string(Token(0x7000_0001), &hi);
        let main = module.add_method(
            MethodBodyImage {
                max_stack: 8,
                init_locals: true,
                local_var_sig: None,
                code: vec![
                    Instruction::new(Opcode::Ldstr, Operand::Token(Token(0x7000_0001))),
                    Instruction::new(Opcode::Call, Operand::Token(Token(0x0A00_0001))),
                    Instruction::simple(Opcode::Ret),
                ]
                .into_boxed_slice(),
                handlers: <Box<[lamella_cil::EhClause]>>::default(),
            },
            0,
        );
        (module, main)
    }

    fn request_frames(commands: &[&str]) -> Vec<u8> {
        let mut input = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            let message = Message::Request(Request {
                seq: index as i64 + 1,
                command: (*command).to_owned(),
                arguments: None,
            });
            write_message(&mut input, &message).unwrap();
        }
        input
    }

    fn read_all(bytes: Vec<u8>) -> Vec<Message> {
        let mut reader = Cursor::new(bytes);
        let mut messages = Vec::new();
        while let Some(message) = read_message(&mut reader).unwrap() {
            messages.push(message);
        }
        messages
    }

    #[test]
    fn serves_a_full_scripted_session() {
        let (module, main) = program();
        let mut debugger = Debugger::new(module, main);

        let input = request_frames(&["initialize", "launch", "continue", "disconnect"]);
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        serve(&mut debugger, &mut reader, &mut output).unwrap();

        let messages = read_all(output);
        let responses = messages
            .iter()
            .filter(|m| matches!(m, Message::Response(r) if r.success))
            .count();
        assert_eq!(responses, 4);
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "initialized"))
        );
        assert!(
            messages
                .iter()
                .any(|m| matches!(m, Message::Event(e) if e.event == "terminated"))
        );
        assert_eq!(debugger.output_string(), "hi\n");
    }

    #[test]
    fn stops_at_a_clean_end_of_stream_without_disconnect() {
        let (module, main) = program();
        let mut debugger = Debugger::new(module, main);
        let input = request_frames(&["initialize"]);
        let mut reader = Cursor::new(input);
        let mut output = Vec::new();
        serve(&mut debugger, &mut reader, &mut output).unwrap();
        assert!(!read_all(output).is_empty());
    }
}
