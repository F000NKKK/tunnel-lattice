# Tunnel Lattice

**Языки**

🇺🇸 [English](README.md) | 🇷🇺 **Русский**

[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](LICENSE)
[![Rust](https://img.shields.io/badge/language-Rust-orange.svg)](https://www.rust-lang.org)
[![crates.io](https://img.shields.io/crates/v/tunnel-lattice.svg)](https://crates.io/crates/tunnel-lattice)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice)](https://docs.rs/tunnel-lattice)
[![Downloads](https://img.shields.io/crates/d/tunnel-lattice.svg)](https://crates.io/crates/tunnel-lattice)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](Cargo.toml)
[![CI](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml/badge.svg)](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml)

![Linux](https://img.shields.io/badge/Linux-in%20progress-yellow)
![Windows](https://img.shields.io/badge/Windows-in%20progress-yellow)
![macOS](https://img.shields.io/badge/macOS-in%20progress-yellow)

Кроссплатформенная Rust-библиотека для туннельных интерфейсов TUN/TAP,
рассчитанная на совместную работу с остальным стеком Lattice.

## Статус

**Пререлиз, идёт активное проектирование/реализация.** Релиз ещё не
опубликован (см. `SUPPORT.md`). В воркспейсе уже есть реальная архитектура
крейтов — см. [ARCHITECTURE.ru.md](ARCHITECTURE.ru.md), — но ничего в ней
не заморожено по API: любой тип, трейт и Cargo-фича могут измениться до
выхода `0.1.0`.

## Что делает библиотека

- Создаёт и настраивает устройства TUN (сырой IP) и TAP (кадры Ethernet) на
  Linux, Windows и macOS через крейт `tun-rs`;
- Передаёт пакеты через открытое устройство: синхронно по умолчанию и, с
  опциональной фичей `async`, через `futures::Stream` — асинхронный рантайм
  подключается только при включении этой фичи;
- Перечитывает и изменяет MTU и административное состояние открытого
  устройства.

Tunnel Lattice не назначает IP-адреса созданным интерфейсам — за настройку
сети ОС после создания устройства отвечает `net-lattice` (см. ниже).

## Быстрый старт

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; 1500];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes");
    Ok(())
}
```

Флаги фич и более полный обзор использования — в
`crates/tunnel-lattice/README.md`.

## Экосистема Lattice

| Крейт | Назначение |
| --- | --- |
| [net-lattice](https://github.com/F000NKKK/net-lattice) | Инспекция и настройка сетевого стека ОС (маршруты, DNS, интерфейсы) |
| [tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice) | TUN/TAP туннельные интерфейсы |
| [dns-lattice](https://github.com/F000NKKK/dns-lattice) | Программируемый DNS control plane |
| [flow-lattice](https://github.com/F000NKKK/flow-lattice) | Компилятор политик: правила -> платформенно-нейтральные сетевые планы |
| [sdk-lattice](https://github.com/F000NKKK/sdk-lattice) | Прикладной SDK, объединяющий крейты выше |

## Участие в разработке

См. [CONTRIBUTING.md](CONTRIBUTING.md). На этой стадии наиболее ценна
обратная связь по архитектуре крейтов и форме API в
[ARCHITECTURE.ru.md](ARCHITECTURE.ru.md).

## Лицензия

Распространяется под [Mozilla Public License 2.0](LICENSE).
