use crossterm::Command;

#[allow(dead_code)]
pub enum OpsCommandType {
    ClearToEnd,
    UpOneLine,
}

pub struct OpsCommand(pub OpsCommandType);

impl Command for OpsCommand {
    fn write_ansi(&self, f: &mut impl std::fmt::Write) -> std::fmt::Result {
        f.write_str(match self.0 {
            OpsCommandType::ClearToEnd => "\x1b[K",
            OpsCommandType::UpOneLine => "\x1b[F",
        })
    }

    #[cfg(windows)]
    fn execute_winapi(&self) -> io::Result<()> {
        sys::clear(self.0)
    }
}
