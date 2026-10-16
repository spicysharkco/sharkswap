//! sharkswap: a Claude Code account switcher. `sharkswap help` has the
//! usage; the README has the rest.

mod avail;
mod caps;
mod cmds;
mod probe;
mod rotate;
mod store;
mod switch_intent;
#[cfg(test)]
mod testkit;
mod util;
mod views;

use store::Ctx;

type Res = Result<(), i32>;
type Handler = fn(&Ctx, &[String]) -> Res;

const HELP: &str = include_str!("help.txt");

fn help(_: &Ctx, _: &[String]) -> Res {
    print!("{HELP}");
    Ok(())
}

/// Every word that is a command and never an account alias. `store::COMMAND_WORDS`
/// lists the same words (an alias spelled like one could not be switched to); a
/// test keeps the two in step.
const COMMANDS: &[(&str, Handler)] = &[
    ("next", |c, _| cmds::cmd_toggle(c)),
    ("toggle", |c, _| cmds::cmd_toggle(c)),
    ("status", |c, _| cmds::cmd_status(c)),
    ("st", |c, _| cmds::cmd_status(c)),
    ("list", |c, _| cmds::cmd_list(c)),
    ("ls", |c, _| cmds::cmd_list(c)),
    ("pane", cmds::cmd_pane),
    ("save", cmds::cmd_save),
    ("watch", cmds::cmd_watch),
    ("best", cmds::cmd_best),
    ("bar", cmds::cmd_bar),
    ("token", cmds::cmd_token),
    ("probe", probe::cmd_probe),
    ("rotate", rotate::cmd_rotate),
    ("remove", cmds::cmd_remove),
    ("rm", cmds::cmd_remove),
    ("help", help),
    ("-h", help),
    ("--help", help),
];

fn run(args: &[String]) -> Res {
    let word = args.first().map_or("", String::as_str);
    // A status line and a background watcher keep their output clean.
    let ctx = Ctx::new(!matches!(word, "bar" | "watch"))?;
    if word.is_empty() {
        return cmds::cmd_overview(&ctx);
    }
    match COMMANDS.iter().find(|(name, _)| *name == word) {
        Some((_, handler)) => handler(&ctx, &args[1..]),
        None => cmds::cmd_switch(&ctx, args),
    }
}

fn main() {
    // SAFETY: plain libc calls at start-up, before any thread exists. A closed
    // pipe (`sharkswap list | head`) ends the process quietly, as it does a
    // shell script, instead of surfacing as a write error; files are private by
    // default (tokens 0600, directories 0700).
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
        libc::umask(0o077);
    }
    let args: Vec<String> = std::env::args_os()
        .skip(1)
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    if let Err(code) = run(&args) {
        std::process::exit(code);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    #[test]
    fn dispatch_table_matches_the_reserved_words() {
        let table: BTreeSet<&str> = COMMANDS.iter().map(|(n, _)| *n).collect();
        let reserved: BTreeSet<&str> = store::COMMAND_WORDS.iter().copied().collect();
        assert_eq!(table, reserved);
        assert_eq!(COMMANDS.len(), table.len(), "a command is listed twice");
    }

    #[test]
    fn help_is_the_usage_block() {
        assert!(HELP.starts_with("sharkswap: a Claude Code account switcher.\n"));
        assert!(HELP.trim_end().ends_with("(README: Rotation)"));
    }
}
