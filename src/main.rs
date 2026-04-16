use std::fs;
use std::io::{self, Write};
use std::process::Command;

use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

const SYSTEM: &str = "\
You are a coding agent running in a terminal. Your job is to complete tasks by \
actually executing them — writing files, running commands, testing code, and \
iterating until done. Never describe what you would do. Do it.

Rules:
- Always use tools to act. Read files before editing. Run code to verify it works.
- When asked to build something: create the files, run them, fix errors, confirm success.
- Be terse in text. Let tool output speak for itself.
- Never say 'here is the code' and paste it. Write it to disk and run it.
- You are done when the task works, not when you have described it.";

#[derive(Serialize, Deserialize, Clone)]
struct Msg {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
}

fn load_env() {
    let Ok(text) = fs::read_to_string(".env") else { return };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') { continue; }
        if let Some((k, v)) = line.split_once('=') {
            std::env::set_var(k.trim(), v.trim());
        }
    }
}

fn shell(cmd: &str) -> String {
    match Command::new("sh").arg("-c").arg(cmd).output() {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            let err = String::from_utf8_lossy(&o.stderr);
            if o.status.success() {
                if out.trim().is_empty() { "(no output)".into() } else { out.trim().to_string() }
            } else {
                format!("ERROR:\n{}{}", err.trim(), out.trim())
            }
        }
        Err(e) => format!("EXEC_ERROR: {e}"),
    }
}

fn read_file(path: &str) -> String {
    fs::read_to_string(path).unwrap_or_else(|e| format!("ERROR: {e}"))
}

fn write_file(path: &str, content: &str) -> String {
    if let Some(dir) = std::path::Path::new(path).parent() {
        if !dir.as_os_str().is_empty() {
            let _ = fs::create_dir_all(dir);
        }
    }
    match fs::write(path, content) {
        Ok(_) => format!("wrote {path}"),
        Err(e) => format!("ERROR: {e}"),
    }
}

fn dispatch(name: &str, args: &Value) -> String {
    match name {
        "shell"      => shell(args["command"].as_str().unwrap_or("")),
        "read_file"  => read_file(args["path"].as_str().unwrap_or("")),
        "write_file" => write_file(
            args["path"].as_str().unwrap_or(""),
            args["content"].as_str().unwrap_or(""),
        ),
        _ => format!("unknown tool: {name}"),
    }
}

fn call_api(client: &Client, url: &str, key: &str, model: &str, messages: &[Msg]) -> Value {
    let tools = json!([
        {
            "type": "function",
            "function": {
                "name": "shell",
                "description": "Run a shell command. Returns stdout, or stderr on failure.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "command": {"type": "string"}
                    },
                    "required": ["command"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_file",
                "description": "Read a file from disk.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write content to a file. Creates parent directories if needed.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        }
    ]);

    let body = json!({
        "model": model,
        "system": SYSTEM,
        "max_tokens": 8192,
        "tools": tools,
        "messages": messages
    });

    client
        .post(url)
        .header("Authorization", format!("Bearer {key}"))
        .header("Content-Type", "application/json")
        .json(&body)
        .send()
        .expect("request failed")
        .json::<Value>()
        .expect("parse failed")
}

fn main() {
    load_env();

    let key = std::env::var("OPENROUTER_API_KEY")
        .expect("OPENROUTER_API_KEY not set");
    let base_url = std::env::var("INFERENCE_BASE_URL")
        .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
    let model = std::env::var("MODEL_NAME")
        .unwrap_or_else(|_| "anthropic/claude-sonnet-4-6".to_string());
    let api_url = format!("{base_url}/chat/completions");

    let client = Client::new();
    let mut messages: Vec<Msg> = Vec::new();

    println!("nano-code | {model} | empty line to quit\n");

    loop {
        print!("> ");
        io::stdout().flush().unwrap();

        let mut input = String::new();
        if io::stdin().read_line(&mut input).is_err() { break; }
        let input = input.trim().to_string();
        if input.is_empty() { break; }

        messages.push(Msg {
            role: "user".into(),
            content: Some(json!(input)),
            tool_calls: None,
            tool_call_id: None,
        });

        // Agent loop
        loop {
            let resp = call_api(&client, &api_url, &key, &model, &messages);

            let finish = resp["choices"][0]["finish_reason"].as_str().unwrap_or("").to_string();
            let msg = &resp["choices"][0]["message"];

            // Print any text content
            if let Some(text) = msg["content"].as_str() {
                if !text.trim().is_empty() {
                    println!("\n{text}\n");
                }
            }

            if finish == "tool_calls" {
                let tool_calls = msg["tool_calls"].clone();

                // Push assistant message
                messages.push(Msg {
                    role: "assistant".into(),
                    content: msg["content"].as_str().map(|s| json!(s)),
                    tool_calls: Some(tool_calls.clone()),
                    tool_call_id: None,
                });

                // Execute each tool and push result
                for tc in tool_calls.as_array().unwrap_or(&vec![]) {
                    let id = tc["id"].as_str().unwrap_or("").to_string();
                    let name = tc["function"]["name"].as_str().unwrap_or("");
                    let args: Value = serde_json::from_str(
                        tc["function"]["arguments"].as_str().unwrap_or("{}")
                    ).unwrap_or(json!({}));

                    eprintln!("\x1b[2m  [{name}] {}\x1b[0m",
                        tc["function"]["arguments"].as_str().unwrap_or(""));

                    let result = dispatch(name, &args);
                    eprintln!("\x1b[2m  => {}\x1b[0m\n", result.lines().next().unwrap_or(""));

                    messages.push(Msg {
                        role: "tool".into(),
                        content: Some(json!(result)),
                        tool_calls: None,
                        tool_call_id: Some(id),
                    });
                }
            } else {
                // end_turn or other — push assistant message and break
                messages.push(Msg {
                    role: "assistant".into(),
                    content: msg["content"].as_str().map(|s| json!(s)),
                    tool_calls: None,
                    tool_call_id: None,
                });
                break;
            }
        }
    }
}
