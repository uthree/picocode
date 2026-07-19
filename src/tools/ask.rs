//! `ask_user` — the model presents options; the TUI shows a selection dialog.

use rig::tool::Tool;
use serde::Deserialize;
use serde_json::json;
use tokio::sync::{mpsc, oneshot};

use super::ToolError;
use crate::event::AgentEvent;

const MAX_OPTIONS: usize = 10;

#[derive(Deserialize)]
pub struct AskArgs {
    question: String,
    options: Vec<String>,
}

pub struct AskUser {
    tx: mpsc::Sender<AgentEvent>,
}

impl AskUser {
    pub fn new(tx: mpsc::Sender<AgentEvent>) -> Self {
        Self { tx }
    }
}

impl Tool for AskUser {
    const NAME: &'static str = "ask_user";
    type Error = ToolError;
    type Args = AskArgs;
    type Output = String;

    fn description(&self) -> String {
        "Ask the user to pick one of several options when you need a decision \
         you cannot make yourself: ambiguous requests, design choices, or \
         which of several approaches to take. A selection dialog is shown; \
         the chosen option is returned. Keep options short and mutually \
         exclusive. Not for tool approval (that is handled separately) or for \
         open-ended questions (ask those in your reply instead)."
            .to_string()
    }

    fn parameters(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "question": {
                    "type": "string",
                    "description": "The question to show the user"
                },
                "options": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "2-10 short answer options to choose from"
                }
            },
            "required": ["question", "options"]
        })
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let options: Vec<String> = args
            .options
            .into_iter()
            .map(|o| o.trim().to_string())
            .filter(|o| !o.is_empty())
            .take(MAX_OPTIONS)
            .collect();
        if options.is_empty() {
            return Err(ToolError::new(
                "options must contain at least one non-empty entry",
            ));
        }
        let (respond, rx) = oneshot::channel();
        self.tx
            .send(AgentEvent::UserQuestion {
                title: "Question".to_string(),
                question: args.question,
                options: options.clone(),
                respond,
            })
            .await
            .map_err(|_| ToolError::new("UI channel closed"))?;
        match rx.await {
            Ok(Some(i)) if i < options.len() => Ok(format!("The user chose: {}", options[i])),
            Ok(_) => Ok(
                "The user dismissed the question without choosing. Proceed with \
                         your best judgment, or ask in plain text."
                    .to_string(),
            ),
            Err(_) => Err(ToolError::new("question cancelled")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(question: &str, options: &[&str]) -> AskArgs {
        AskArgs {
            question: question.into(),
            options: options.iter().map(|s| s.to_string()).collect(),
        }
    }

    #[tokio::test]
    async fn returns_the_selected_option() {
        let (tx, mut rx) = mpsc::channel(4);
        let tool = AskUser::new(tx);
        let call = tokio::spawn(async move { tool.call(ask("Color?", &["red", "green"])).await });

        let Some(AgentEvent::UserQuestion {
            question,
            options,
            respond,
            ..
        }) = rx.recv().await
        else {
            panic!("expected a UserQuestion event");
        };
        assert_eq!(question, "Color?");
        assert_eq!(options, ["red", "green"]);
        respond.send(Some(1)).unwrap();

        let out = call.await.unwrap().unwrap();
        assert_eq!(out, "The user chose: green");
    }

    #[tokio::test]
    async fn dismissal_and_empty_options() {
        let (tx, mut rx) = mpsc::channel(4);
        let tool = AskUser::new(tx.clone());
        let call = tokio::spawn(async move { tool.call(ask("Pick", &["a"])).await });
        let Some(AgentEvent::UserQuestion { respond, .. }) = rx.recv().await else {
            panic!("expected a UserQuestion event");
        };
        respond.send(None).unwrap();
        assert!(call.await.unwrap().unwrap().contains("dismissed"));

        let tool = AskUser::new(tx);
        assert!(tool.call(ask("Pick", &["", "  "])).await.is_err());
    }
}
