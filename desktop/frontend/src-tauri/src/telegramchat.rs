use crate::config;
use crate::telegram;
use crate::telegramlink::Link;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Mutex, OnceLock};

const FILE: &str = "telegram-chat.json";
const MAX_PROMPT_CHARS: usize = 12_000;

#[derive(Clone, Default, Deserialize, Serialize)]
struct Conversation {
    project: String,
    #[serde(default)]
    agent: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<String>,
}

#[derive(Default, Deserialize, Serialize)]
struct Conversations {
    #[serde(default)]
    chats: BTreeMap<i64, Conversation>,
}

fn path() -> std::path::PathBuf {
    config::lpm_dir().join(FILE)
}

fn conversations() -> &'static Mutex<Conversations> {
    static STATE: OnceLock<Mutex<Conversations>> = OnceLock::new();
    STATE.get_or_init(|| {
        let loaded = std::fs::read(path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        Mutex::new(loaded)
    })
}

fn running() -> &'static Mutex<BTreeSet<i64>> {
    static RUNNING: OnceLock<Mutex<BTreeSet<i64>>> = OnceLock::new();
    RUNNING.get_or_init(Default::default)
}

fn save(state: &Conversations) -> Result<(), String> {
    std::fs::create_dir_all(config::lpm_dir()).map_err(|e| e.to_string())?;
    let tmp = path().with_extension("json.tmp");
    std::fs::write(
        &tmp,
        serde_json::to_vec_pretty(state).map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    std::fs::rename(tmp, path()).map_err(|e| e.to_string())
}

fn say(token: &str, chat: i64, title: &str, body: impl AsRef<str>) {
    let _ = telegram::send_chat_message(token, chat, title, body.as_ref());
}

fn help(token: &str, chat: i64) {
    say(
        token,
        chat,
        "lpm agent chat",
        "Choose a project with /projects then /use <project>. Send a normal message to talk to the agent.\n\n/new starts a fresh conversation. /agent claude or /agent codex picks the agent. Telegram turns are read-only. /status shows this chat's selection.",
    );
}

fn projects() -> Vec<String> {
    config::try_project_names()
        .unwrap_or_default()
        .into_iter()
        .filter(|name| config::project_root(name).is_ok_and(|(_, remote)| !remote))
        .collect()
}

fn show_projects(token: &str, chat: i64) {
    let projects = projects();
    if projects.is_empty() {
        say(
            token,
            chat,
            "No local projects",
            "Add a local project in lpm first.",
        );
    } else {
        let lines = projects
            .iter()
            .map(|name| format!("• `{name}`"))
            .collect::<Vec<_>>()
            .join("\n");
        say(
            token,
            chat,
            "Projects",
            format!("{lines}\n\nChoose one with /use <project>."),
        );
    }
}

fn select_project(token: &str, chat: i64, raw: &str) {
    let project = raw.trim();
    if project.is_empty() {
        say(
            token,
            chat,
            "Choose a project",
            "Use /projects, then /use <project>.",
        );
        return;
    }
    if !projects().iter().any(|name| name == project) {
        say(
            token,
            chat,
            "Unknown project",
            "Use /projects to see the local projects this bot can use.",
        );
        return;
    }
    let mut state = conversations().lock().unwrap_or_else(|e| e.into_inner());
    let conversation = state.chats.entry(chat).or_default();
    conversation.project = project.to_string();
    conversation.session = None;
    if let Err(error) = save(&state) {
        say(token, chat, "Couldn't save selection", error);
        return;
    }
    say(
        token,
        chat,
        "Project selected",
        format!("Using `{project}`. Send a message when you're ready."),
    );
}

fn select_agent(token: &str, chat: i64, raw: &str) {
    let agent = raw.trim().to_lowercase();
    if !matches!(agent.as_str(), "claude" | "codex") {
        say(
            token,
            chat,
            "Choose an agent",
            "Use /agent claude or /agent codex. Telegram turns are read-only.",
        );
        return;
    }
    let mut state = conversations().lock().unwrap_or_else(|e| e.into_inner());
    let conversation = state.chats.entry(chat).or_default();
    conversation.agent = agent.clone();
    conversation.session = None;
    if let Err(error) = save(&state) {
        say(token, chat, "Couldn't save selection", error);
        return;
    }
    say(
        token,
        chat,
        "Agent selected",
        format!("Using `{agent}` for new turns."),
    );
}

fn status(token: &str, chat: i64) {
    let state = conversations().lock().unwrap_or_else(|e| e.into_inner());
    let Some(conversation) = state.chats.get(&chat) else {
        help(token, chat);
        return;
    };
    let project = if conversation.project.is_empty() {
        "none"
    } else {
        &conversation.project
    };
    let agent = if conversation.agent.is_empty() {
        "lpm default"
    } else {
        &conversation.agent
    };
    let continuation = if conversation.session.is_some() {
        "resumable"
    } else {
        "new session next turn"
    };
    say(
        token,
        chat,
        "Telegram agent chat",
        format!("Project: `{project}`\nAgent: `{agent}`\nConversation: {continuation}"),
    );
}

fn new_conversation(token: &str, chat: i64) {
    let mut state = conversations().lock().unwrap_or_else(|e| e.into_inner());
    let conversation = state.chats.entry(chat).or_default();
    conversation.session = None;
    if let Err(error) = save(&state) {
        say(token, chat, "Couldn't reset conversation", error);
        return;
    }
    say(
        token,
        chat,
        "New conversation",
        "The next message will start a fresh agent session.",
    );
}

fn command(text: &str) -> Option<(String, &str)> {
    let text = text.trim();
    let command = text.strip_prefix('/')?;
    let (name, rest) = command
        .split_once(char::is_whitespace)
        .unwrap_or((command, ""));
    Some((
        name.split('@').next().unwrap_or_default().to_lowercase(),
        rest.trim(),
    ))
}

fn start_turn(token: &str, chat: i64, prompt: &str) {
    if prompt.chars().count() > MAX_PROMPT_CHARS {
        say(
            token,
            chat,
            "Message is too long",
            format!("Keep one message under {MAX_PROMPT_CHARS} characters."),
        );
        return;
    }
    let conversation = {
        let state = conversations().lock().unwrap_or_else(|e| e.into_inner());
        state.chats.get(&chat).cloned().unwrap_or_default()
    };
    if conversation.project.is_empty() {
        say(
            token,
            chat,
            "Choose a project first",
            "Use /projects, then /use <project>.",
        );
        return;
    }
    if !running()
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(chat)
    {
        say(
            token,
            chat,
            "Still working",
            "Wait for the current agent reply before sending the next message.",
        );
        return;
    }
    say(
        token,
        chat,
        "Thinking…",
        format!(
            "Asking `{}` in `{}`.",
            if conversation.agent.is_empty() {
                "lpm default"
            } else {
                &conversation.agent
            },
            conversation.project
        ),
    );
    let token = token.to_string();
    let prompt = prompt.to_string();
    std::thread::spawn(move || {
        let outcome = crate::jobs::run_chat_agent(
            &conversation.project,
            &prompt,
            &conversation.agent,
            conversation.session.as_deref(),
        );
        match outcome {
            Ok(reply) => {
                let mut state = conversations().lock().unwrap_or_else(|e| e.into_inner());
                if let Some(current) = state.chats.get_mut(&chat) {
                    current.session = reply.session;
                    let _ = save(&state);
                }
                drop(state);
                say(&token, chat, "Agent reply", reply.result);
            }
            Err(error) => say(&token, chat, "Agent couldn't reply", error),
        }
        running()
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&chat);
    });
}

/// Handles normal private messages from the Telegram owner. Automation answers
/// are handled first in telegramanswer.rs and never reach this router.
pub fn handle(token: &str, message: &Value) -> bool {
    let (Some(chat), Some(from), Some(text)) = (
        message.pointer("/chat/id").and_then(Value::as_i64),
        message.pointer("/from/id").and_then(Value::as_i64),
        message.get("text").and_then(Value::as_str),
    ) else {
        return false;
    };
    let link = Link::load();
    if !link.is_owner(from) || !link.is_private_chat(chat) {
        return false;
    }
    if let Some((name, rest)) = command(text) {
        match name.as_str() {
            "start" | "help" => help(token, chat),
            "projects" => show_projects(token, chat),
            "use" => select_project(token, chat, rest),
            "agent" => select_agent(token, chat, rest),
            "new" => new_conversation(token, chat),
            "status" => status(token, chat),
            _ => say(
                token,
                chat,
                "Unknown command",
                "Use /help to see the available commands.",
            ),
        }
    } else {
        start_turn(token, chat, text);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_commands_with_or_without_a_bot_name() {
        assert_eq!(command("/use lpm"), Some(("use".to_string(), "lpm")));
        assert_eq!(
            command("/projects@my_bot"),
            Some(("projects".to_string(), ""))
        );
        assert_eq!(command("hello"), None);
    }
}
