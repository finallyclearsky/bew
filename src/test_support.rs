use crate::model::Bd;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// An executable stand-in for `bd` that appends its arguments, one per line,
/// to a temp file and then runs a shell body. Both files go away on drop.
pub struct FakeBd {
    script: PathBuf,
    arguments: PathBuf,
}

impl FakeBd {
    /// A `bd` that prints `stdout` whatever it is asked.
    pub fn new(tag: &str, stdout: &str) -> Self {
        Self::script(tag, &format!("printf '%s\\n' '{stdout}'"))
    }

    /// A `bd` that runs `body` as `sh`; `$*` holds the arguments.
    pub fn script(tag: &str, body: &str) -> Self {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir();
        let pid = std::process::id();
        let script = directory.join(format!("be-fake-bd-{tag}-{pid}-{nonce}"));
        let arguments = directory.join(format!("be-fake-bd-{tag}-args-{pid}-{nonce}"));
        fs::write(
            &script,
            format!(
                "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\n{body}\n",
                arguments.display()
            ),
        )
        .unwrap();
        let mut permissions = fs::metadata(&script).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(&script, permissions).unwrap();
        Self { script, arguments }
    }

    pub fn bd(&self, db: Option<PathBuf>) -> Bd {
        Bd::new(self.script.clone().into_os_string(), db)
    }

    pub fn arguments(&self) -> Vec<String> {
        fs::read_to_string(&self.arguments)
            .unwrap_or_default()
            .lines()
            .map(str::to_string)
            .collect()
    }
}

impl Drop for FakeBd {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.script);
        let _ = fs::remove_file(&self.arguments);
    }
}
