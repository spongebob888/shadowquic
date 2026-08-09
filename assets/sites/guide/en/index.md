# Beginner's Guide

Welcome! This guide explains **ShadowQUIC** in plain words — no network
background required. If you have never used a proxy tool before, start here.

ShadowQUIC is a proxy tool that runs on two machines:

- a **server** on a remote machine (where you have control / a public IP),
- a **client** on your local machine (laptop, phone, router).

It builds a fast, hard-to-block tunnel between them using QUIC, the same
transport protocol that powers HTTP/3. Because the tunnel looks like ordinary
encrypted web traffic, it is very hard for a firewall to tell it apart from a
normal website visit.

## What you will learn

| Page | What it covers |
|------|----------------|
| [What is ShadowQUIC](what-is.md) | The big picture, in plain words |
| [Quick Start](quickstart.md) | Get client + server running in 5 minutes |
| [Configuration explained](configuration.md) | Every line of the config file, translated into human language |
| [FAQ](faq.md) | Answers to the most common questions |
