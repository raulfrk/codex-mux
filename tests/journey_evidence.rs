use std::{fs::OpenOptions, io::Write};

pub struct JourneyEvidence(&'static [&'static str]);

pub fn journey(actions: &'static [&'static str]) -> JourneyEvidence {
    JourneyEvidence(actions)
}

impl Drop for JourneyEvidence {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        let Some(path) = std::env::var_os("CODEX_MUX_JOURNEY_EVIDENCE") else {
            return;
        };
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .expect("open journey evidence log");
        for action in self.0 {
            writeln!(file, "{action}").expect("append journey evidence");
        }
    }
}
