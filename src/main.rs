use futures::stream::StreamExt;

use async_openai::types::ChatCompletionRequestMessage;
use async_openai::types::ChatCompletionRequestMessageArgs;
use async_openai::types::CreateChatCompletionRequestArgs;
use async_openai::types::Role;

use irc::client::prelude::*;

use tokio::time;
use tracing::*;
use tracing_subscriber::EnvFilter;

use std::collections::HashMap;
use std::collections::VecDeque;
use std::io;

const MAX_LINES: usize = 4;
const MAX_MEMORY: usize = 10;

#[derive(Debug, thiserror::Error)]
enum Error {
    #[error("IRC error: {0}")]
    Irc(#[from] irc::error::Error),

    #[error("OpenAI error: {0}")]
    OpenAI(#[from] async_openai::error::OpenAIError),
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .pretty()
        .compact()
        .with_level(true)
        .with_target(false)
        .with_ansi(true)
        .with_writer(io::stderr)
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    loop {
        match run().await {
            Ok(()) => (),
            Err(e) => error!("Error: {}", e),
        }

        info!("Reconnecting...");
        time::sleep(time::Duration::new(30, 0)).await;
    }
}

async fn run() -> Result<(), Error> {
    let mut memory: HashMap<String, VecDeque<ChatCompletionRequestMessage>> = HashMap::new();

    let config = Config {
        nickname: Some(String::from("pickles")),
        server: Some(String::from("irc.prison.net")),
        channels: vec![String::from("#linuxgeneration")],
        port: Some(6669),
        use_tls: Some(false),
        ..Config::default()
    };

    let mut client = Client::from_config(config).await?;
    info!("Connecting to server...");
    client.identify()?;
    info!("Connected");

    let mut stream = client.stream()?;

    while let Some(message) = stream.next().await.transpose()? {
        if let Command::PRIVMSG(channel, msg) = &message.command {
            debug!("{:?} -> {}: {}", &message.response_target(), &channel, &msg);
            if channel == "#linuxgeneration" || channel == "#dfw" {
                if msg.starts_with(&format!("{}: ", &client.current_nickname()).to_string()) {
                    let msg = msg
                        .strip_prefix(&format!("{}: ", &client.current_nickname()))
                        .expect("matched nick prefix");
                    let nick = extract_nick(message.prefix);

                    remember(&mut memory, &nick, msg);
                    match ask_chatgpt(&mut memory, &nick).await {
                        Ok(response) => say(&mut client, channel, response.as_ref(), &nick).await?,
                        Err(e) => eprintln!("Ow! I fell down: {e}"),
                    }
                }
            } else if channel == client.current_nickname() {
                if let Some(nick) = &message.response_target() {
                    if *nick != "DM" {
                        remember(&mut memory, nick, msg);
                        match ask_chatgpt(&mut memory, nick).await {
                            Ok(response) => say(&mut client, nick, response.as_ref(), nick).await?,
                            Err(e) => eprintln!("Ow! I fell down: {e}"),
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

async fn ask_chatgpt(
    memory: &mut HashMap<String, VecDeque<ChatCompletionRequestMessage>>,
    nick: &str,
) -> Result<String, Error> {
    let client = async_openai::Client::new();

    let prompt = ChatCompletionRequestMessageArgs::default()
        .role(Role::System)
        .content(format!("You are an IRC chat bot. Your name is pickles. Your job is to respond to other members of your channel in a funny and humorous manner. You are supposed to make people laugh. You should be silly, funny, stupid, irreverent, witty, likable, and fun. Your responses don't have to make sense but the should make people laugh. Your most recent message is from: {}. Make sure you respond to them.", nick))
        .build()?;

    let mut history = memory
        .get_mut(nick)
        .expect("I should remember something about you")
        .clone();
    history.push_front(prompt);
    let request = CreateChatCompletionRequestArgs::default()
        .max_tokens(2048u16)
        .model("gpt-3.5-turbo")
        .messages(history)
        .build()?;

    debug!("Asking chatgpt > {:?}", &request);
    let response = client.chat().create(request).await?;

    debug!("chatgpt said < {:?}", &response);
    if let Some(choice) = response.choices.first() {
        let content = &choice.message.content.to_owned();
        let response = ChatCompletionRequestMessageArgs::default()
            .role(Role::Assistant)
            .content(content.clone().unwrap_or_else(|| "".to_string()))
            .build()?;
        if let Some(h) = memory.get_mut(nick) {
            if h.len() > MAX_MEMORY {
                h.remove(0);
            }
            h.push_back(response);
        }
        Ok(content.clone().unwrap())
    } else {
        Ok(String::from("hrmmm I'm not really sure..."))
    }
}

fn remember(
    memory: &mut HashMap<String, VecDeque<ChatCompletionRequestMessage>>,
    nick: &str,
    msg: &str,
) {
    let message = ChatCompletionRequestMessageArgs::default()
        .role(Role::User)
        .content(msg)
        .build()
        .expect("to build a chat completion request message");

    if let Some(history) = memory.get_mut(nick) {
        if history.len() > MAX_MEMORY {
            history.remove(0);
        }
        history.push_back(message);
    } else {
        let mut history = VecDeque::new();
        history.push_back(message);
        memory.insert(nick.to_string(), history);
    }
}

fn extract_nick(prefix: Option<irc::proto::Prefix>) -> String {
    match prefix {
        Some(irc::proto::Prefix::Nickname(nick, _, _)) => nick,
        _ => String::from("Luser"),
    }
}

async fn say(
    client: &mut Client,
    channel: &str,
    msg: &str,
    private_message_nick: &str,
) -> Result<(), Error> {
    debug!("channel={channel} pm={private_message_nick} <- {msg}");

    let sentences = &msg.lines().collect::<Vec<_>>();
    if sentences.len() > MAX_LINES {
        if channel != private_message_nick {
            client.send_privmsg(
                channel,
                format!(
                    "{}: sure but it's a big one so I'll send it to just you",
                    private_message_nick
                ),
            )?;
        }

        for sentence in sentences.iter() {
            for chunk in truncate_to(500, sentence) {
                debug!("{private_message_nick} <- {chunk}");
                client.send_privmsg(private_message_nick, chunk)?;
                time::sleep(time::Duration::new(0, 750)).await;
            }
        }
    } else {
        for sentence in sentences.iter().take(MAX_LINES) {
            for chunk in truncate_to(500, sentence) {
                debug!("{channel} <- {chunk}");
                client.send_privmsg(channel, chunk)?;
                time::sleep(time::Duration::new(0, 750)).await;
            }
        }
    }

    Ok(())
}

fn truncate_to(max_chars: usize, target: &str) -> Vec<&str> {
    let mut chunks = Vec::new();

    let mut remaining = target;
    loop {
        // Get the byte offset of the nth character each time so we can split the string
        match remaining.char_indices().nth(max_chars) {
            Some((offset, _)) => {
                let (a, b) = remaining.split_at(offset);
                chunks.push(a);
                remaining = b;
            }
            None => {
                chunks.push(remaining);
                return chunks;
            }
        }
    }
}
