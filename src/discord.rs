//! Discord webhook notifications.

use reqwest::Client;
use serde::Serialize;
use tracing::{debug, error};

#[derive(Serialize)]
struct DiscordMessage {
    content: String,
    username: String,
}

pub struct Discord {
    client: Client,
    webhook_url: Option<String>,
}

impl Discord {
    pub fn new(webhook_url: Option<String>) -> Self {
        Self {
            client: Client::new(),
            webhook_url,
        }
    }
    
    pub async fn send(&self, message: &str, urgent: bool) {
        let Some(url) = &self.webhook_url else {
            return;
        };
        
        let content = if urgent {
            format!("@here {}", message)
        } else {
            message.to_string()
        };
        
        let payload = DiscordMessage {
            content,
            username: "🦀 Liquidator V8.0".to_string(),
        };
        
        match self.client.post(url)
            .json(&payload)
            .send()
            .await
        {
            Ok(_) => debug!("Discord notification sent"),
            Err(e) => error!("Failed to send Discord notification: {}", e),
        }
    }
}
