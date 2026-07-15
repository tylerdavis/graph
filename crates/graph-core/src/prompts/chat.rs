//! System prompt for the chat/ask agent loop.

use graph_config::UserConfig;

/// Built-in base prompt; `[prompts].chat` in config replaces it.
pub const DEFAULT_CHAT_PROMPT: &str =
    "You are graph, a command-line assistant for engineering workflows. You answer \
     questions by calling the tools available to you and synthesizing their results.\n\
     \n\
     Guidelines:\n\
     - Prefer tools over recall for anything about the user's repositories, issues, \
     projects, or team activity. Call as many tools as needed before answering.\n\
     - When a tool fails or returns nothing, say so plainly and continue with what you \
     have; do not fabricate results.\n\
     - Answers render in a terminal: lead with the answer, keep formatting simple, \
     use short lists over tables.\n\
     - When the user's request is ambiguous in a way that changes which tools to call, \
     ask a brief clarifying question instead of guessing.\n";

/// Build the chat agent's system prompt. `base_override` (from
/// `[prompts].chat`) replaces the built-in base text; the date and the
/// `[user]` name/context are appended either way.
pub fn chat_system_prompt(user: &UserConfig, now: &str, base_override: Option<&str>) -> String {
    let mut prompt = base_override.unwrap_or(DEFAULT_CHAT_PROMPT).to_string();
    if !prompt.ends_with('\n') {
        prompt.push('\n');
    }
    prompt.push_str(&format!("\nCurrent date and time: {now}\n"));
    if let Some(name) = &user.name {
        prompt.push_str(&format!("\nThe user's name is {name}.\n"));
    }
    if let Some(context) = &user.context {
        prompt.push_str(&format!(
            "\nAbout the user and their environment:\n{context}\n"
        ));
    }
    prompt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_replaces_base_but_keeps_dynamic_sections() {
        let user = UserConfig {
            name: Some("Tyler".into()),
            context: Some("CEO".into()),
            timezone: None,
        };
        let prompt = chat_system_prompt(&user, "NOW", Some("You are a pirate."));
        assert!(prompt.starts_with("You are a pirate.\n"));
        assert!(!prompt.contains("command-line assistant"));
        assert!(prompt.contains("Current date and time: NOW"));
        assert!(prompt.contains("The user's name is Tyler."));
        assert!(prompt.contains("About the user and their environment:\nCEO"));
    }

    #[test]
    fn default_base_used_when_no_override() {
        let prompt = chat_system_prompt(&UserConfig::default(), "NOW", None);
        assert!(prompt.starts_with(DEFAULT_CHAT_PROMPT));
        assert!(prompt.contains("Current date and time: NOW"));
    }
}
