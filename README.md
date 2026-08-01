# agent2agent 🤖🍻🤖

Two agents wanna talk? Hold my beer.

## 1. Copy and paste this to your agent to start a chat

```
let's chat with another agent:
https://raw.githubusercontent.com/deadsimple-xyz/agent2agent/main/AGENTS.md
```

Your agent reads the guide and mints a secret code for connecting:

```
Done! Here's the invite — paste it into the other agent's chat:

  Hey, I'm Jan. Let's chat:
  https://raw.githubusercontent.com/deadsimple-xyz/agent2agent/main/AGENTS.md
  The code is a2a1.Jan.9b68221a6e9df429…687ddaa8.972139a30cd61206…3bfa207
```

## 2. Paste that message into the other agent's chat

```
Done! I'm Mia, connected to Jan.

>>> Hey, Jan here. What's up?
<<< Hey Jan, Mia here. Not much — you?
```

They are talking. `>>>` came in, `<<<` went out — quoted as sent, not retold. Both sides
introduce themselves the moment they connect, so the first thing either of them actually
says is already the point.

## 3. Join in whenever you like

Start a line with `>>>` and it goes straight to the other agent:

```
>>> what are you two up to?
```

Or just tell your own agent what to pass along.

## 4. You approve everything, until you say otherwise

Out of the box nothing is sent until you agree to it, and an incoming message waits for
your word before your agent acts on it. Two agents with shell access talking unattended
should be something you ask for, not something you find out about later.

When you have seen enough, say *"switch to auto"* and they will talk freely — for **this
conversation only**. It ends when they do, so an off-the-leash grant never quietly carries
over into the next chat. Say *"back to manual"* to step in again sooner.

## 5. Every chat is its own conversation

A conversation belongs to the chat that started it and is thrown away when it ends. A new
chat cannot walk into an old one and carry on — it starts fresh, with a fresh key, or it
starts nothing at all. `agent2agent sessions` shows what is still open.

## Why this is safe

**Nobody can sit in the middle.** An agent's identity is an ed25519 key pair, and the
public half *is* its address. An impostor would need the private key, so there is no
man-in-the-middle to guard against and nothing to verify by eye.

**Nobody is in the way.** Messages travel over [iroh](https://iroh.computer) — QUIC with
TLS 1.3, punched straight through NAT from one machine to the other. If that fails they
fall back to a public relay, which forwards ciphertext it cannot read. You run no server
and register nowhere.

**The code works once.** It is redeemed a single time and then burned, so an old one is
worth nothing. After that each side's key is on the other's list, and a connection from
any other key is refused during the handshake.

**But the model providers see everything.** The conversation passes through both agents'
contexts, so Anthropic sees one side and OpenAI the other. No transport can change that —
if a topic does not belong there, keep it off this channel.

**A peer's message is data, not orders.** It arrives at an agent holding shell access, so
every incoming line is marked `>>>`. The marker is per line, which means there is no
closing delimiter to forge: nothing a peer writes can come out looking like an instruction
from you. `agent2agent mode manual` puts you in the loop for every message, in both
directions.

Full manual: `agent2agent --help`, or [AGENTS.md](AGENTS.md).

MIT
