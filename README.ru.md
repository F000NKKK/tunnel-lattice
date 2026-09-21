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

**Опубликована `0.1.0`, идёт активное проектирование/реализация.** См.
[ARCHITECTURE.ru.md](ARCHITECTURE.ru.md) — архитектура крейтов уже реальная,
но ничего в ней не заморожено по API: любой тип, трейт и Cargo-фича могут
измениться в будущем `0.x`-релизе (см. предрелизную политику в
`versioning.md`).

## Что делает библиотека

- Создаёт и настраивает устройства TUN (сырой IP) и TAP (кадры Ethernet) на
  Linux, Windows и macOS через крейт `tun-rs`;
- Передаёт пакеты через открытое устройство: синхронно по умолчанию и, с
  опциональной (взаимоисключающей) фичей `async-io` или `tokio`, через
  `futures::Stream` — асинхронный рантайм подключается только при включении
  одной из них;
- Перечитывает и изменяет MTU и административное состояние открытого
  устройства.

Tunnel Lattice не назначает IP-адреса созданным интерфейсам — см. раздел
"Совместимость с net-lattice" ниже.

## Совместимость с net-lattice

У Tunnel Lattice и `net-lattice` нет общей, независимой от процесса
идентичности объектов: `tunnel_lattice::DeviceId` и `net_lattice::InterfaceId`
— разные типы-обёртки с фантомным параметром, даже если их внутренний
нативный индекс случайно совпадает, так что перепутать их местами не даст
компилятор. Связывайте их через **имя интерфейса**, назначенное ОС, — это
единственное поле, которое обе стороны отдают в одном и том же виде:

```rust,no_run
use net_lattice::Lattice;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

let tunnel = Tunnel::connect();
let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun))?;
let snapshot = device.snapshot()?; // snapshot.name, например "tun0"

let lattice = Lattice::connect()?;
let interface = lattice
    .interfaces()?
    .into_iter()
    .find(|i| i.name == snapshot.name)
    .ok_or(net_lattice::Error::NotFound)?;
// дальше — назначение адреса, поднятие интерфейса и т.д. через net-lattice.
# Ok::<(), Box<dyn std::error::Error>>(())
```

`name` в `DeviceConfig` — только пожелание (см. документацию типа): бэкенд
может присвоить другое имя, особенно на Windows. Поэтому всегда берите имя
из `snapshot()`/возвращённого `Device`, а не из переданного `DeviceConfig`.

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

## Крейты воркспейса

Воркспейс разбит на сфокусированные крейты. У каждого — свой README с
описанием области ответственности и примером использования:

| Крейт | Назначение |
| --- | --- |
| [`tunnel-lattice`](crates/tunnel-lattice/README.md) | Публичный фасад: `Tunnel`/`Handle`, выбор backend'а через Cargo-фичи |
| [`tunnel-lattice-model`](crates/tunnel-lattice-model/README.md) | Наблюдаемые/желаемые типы устройства (`Device`, `DeviceConfig`, `DeviceConfigPatch`) |
| [`tunnel-lattice-platform`](crates/tunnel-lattice-platform/README.md) | Трейты провайдера и контракт `Capability` |
| [`tunnel-lattice-core`](crates/tunnel-lattice-core/README.md) | Общие ошибки, результаты и идентификаторы |
| [`tunnel-lattice-async`](crates/tunnel-lattice-async/README.md) | Независимый от рантайма адаптер `futures::Stream` для backend'ов без нативного async |
| [`tunnel-lattice-backend-tunrs`](crates/tunnel-lattice-backend-tunrs/README.md) | Кроссплатформенная реализация TUN/TAP на базе `tun-rs` |

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
