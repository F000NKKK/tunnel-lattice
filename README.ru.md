<div align="center">

<a id="top"></a>

# 🕸️ Tunnel Lattice

### Типизированные кроссплатформенные TUN/TAP-интерфейсы для Rust

[![crates.io](https://img.shields.io/crates/v/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![docs.rs](https://img.shields.io/docsrs/tunnel-lattice?cacheSeconds=86400)](https://docs.rs/tunnel-lattice)
[![Downloads](https://img.shields.io/crates/d/tunnel-lattice.svg?cacheSeconds=86400)](https://crates.io/crates/tunnel-lattice)
[![CI](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml/badge.svg)](https://github.com/F000NKKK/tunnel-lattice/actions/workflows/ci.yml)
[![License: MPL 2.0](https://img.shields.io/badge/license-MPL--2.0-blue.svg)](LICENSE)
[![MSRV](https://img.shields.io/badge/MSRV-1.93-lightgrey.svg)](Cargo.toml)

![Linux](https://img.shields.io/badge/Linux-supported-success)
![Windows](https://img.shields.io/badge/Windows-supported-success)
![macOS](https://img.shields.io/badge/macOS-supported-success)

🇺🇸 [English](README.md) | 🇷🇺 **Русский**

[Возможности](#-ключевые-возможности) • [Платформы](#-поддерживаемые-платформы) • [Производительность](#-производительность) • [Установка](#-установка) • [Быстрый старт](#-быстрый-старт) • [Сравнение](#-сравнение)

</div>

---

## 📖 Обзор

**Tunnel Lattice** создаёт виртуальные сетевые интерфейсы TUN (сырой IP) и
TAP (Ethernet) на Linux, Windows и macOS и работает с ними через один
типизированный API на всех платформах. Это туннельный слой сетевого стека
Lattice; адреса и маршруты настраиваются через
[net-lattice](https://github.com/F000NKKK/net-lattice).

### 🎯 Почему Tunnel Lattice?

- **🧭 Типизированные ошибки везде**: любая ошибка ОС превращается в один
  `tunnel_lattice::Error`. `Disconnected`, `BufferTooSmall`,
  `DriverUnavailable` и `PermissionDenied` значат одно и то же на Linux,
  Windows и macOS — не нужно разбирать errno или коды Win32.
- **🛑 Удалённое устройство всегда завершает `recv`**: удаление интерфейса
  завершает ожидающий `recv` или `PacketStream` с `Disconnected` на всех
  трёх ОС и во всех наборах фич, включая асинхронный TAP на macOS и Tokio на
  Linux, где нижележащая библиотека ждёт вечно.
- **📦 Без молчаливой обрезки**: пакет, не влезающий в буфер, отбрасывается
  с ошибкой `BufferTooSmall`. Половину пакета вы не получите никогда.
- **⚡ Async без привязки к рантайму**: `futures::Stream` пакетов на Tokio
  или `async-io`; рантайм подключается только если вы его попросили.
- **♻️ Ноль аллокаций на пакет**: поток читает прямо в переиспользуемый слот
  `PacketPool`. Это закреплено тестом в репозитории.
- **🔌 Заменяемые бэкенды**: трейты провайдера и флаги `Capability` отделяют
  API от реализации. Сейчас под капотом
  [`tun-rs`](https://github.com/tun-rs/tun-rs); нативные бэкенды под каждую
  ОС встанут на его место без изменений в вашем коде.

> **Статус:** опубликована `0.4.0`, идёт активная разработка. До `1.0` API
> не заморожен; см. [ARCHITECTURE.ru.md](ARCHITECTURE.ru.md).

## 🌟 Ключевые возможности

### Основное
- ✅ **TUN и TAP**: устройства сырого IP (уровень 3) и Ethernet (уровень 2)
- ✅ **Sync и async**: блокирующие `recv`/`send` или `futures::Stream` и
  неблокирующий `send_async` с фичей `tokio` или `async-io`
- ✅ **Чтение и изменение**: перечитать имя, MTU, административное
  состояние и MAC-адрес TAP-устройства; поменять MTU, MAC и up/down на
  открытом устройстве
- ✅ **Дешёвое разделение**: `Handle` реализует `Clone`, а `recv`/`send`
  принимают `&self` — одно устройство можно использовать из многих потоков

### Платформенные возможности
- 🐧 **Персистентность на Linux**: устройство переживает завершение процесса
- 🔀 **Multi-queue на Linux**: независимые очереди ядра на одном устройстве
- 🍎 **TAP на macOS**: пары `feth` с ограниченным ожиданием, чтобы
  уничтоженный интерфейс завершал `recv`
- 🪟 **TUN на Windows**: Wintun; отсутствие `wintun.dll` сообщается как
  `DriverUnavailable`

### Удобство разработки
- 🎯 **Честные имена**: `open` отказывает, если не может выдать имя ровно
  как запрошено, и никогда не подхватывает, а потом уничтожает чужой
  интерфейс
- 🧪 **Проверено на реальных устройствах**: привилегированный CI создаёт и
  удаляет настоящие устройства на Linux, Windows и macOS во всех наборах фич
- 🧩 **Флаги возможностей**: спросите устройство, что оно умеет, вместо
  угадывания по ОС

## 💻 Поддерживаемые платформы

| Платформа   | TUN | TAP | Sync | Tokio | async-io | Примечания |
|-------------|:---:|:---:|:----:|:-----:|:--------:|------------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | Персистентные устройства и multi-queue |
| **Windows** | ✅  | ✅  | ✅   | ✅    | ✅       | TUN нужен `wintun.dll`; TAP нужен драйвер tap-windows6 |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | TUN через `utun`, TAP через пары `feth` |

✅ проверено в CI на реальных устройствах.

> Для создания устройства нужны `CAP_NET_ADMIN` на Linux, права
> администратора на Windows или root на macOS.

## 🚀 Производительность

### 🏆 Особенности устройства

- **Ноль аллокаций и копирований на пакет** в потоке: устройство пишет
  прямо в слот пула, а `tests/alloc_count.rs` падает, если устойчивый поток
  начинает аллоцировать.
- **Без рабочего потока на async-пути**: с `tokio` или `async-io` поток
  использует нативный async I/O бэкенда, а его drop отменяет ожидающий
  `recv`.
- **Обратное давление вместо буферизации**: когда все слоты заняты, поток
  ждёт, пока вы освободите один; память ограничена размером пула.
- **Multi-queue на Linux** для распределения трафика по ядрам.

### 📊 Бенчмарки

Микробенчмарки работают на моках устройств в памяти, привилегии не нужны:

```bash
cargo bench -p tunnel-lattice-async                # пути потока против простого цикла recv
cargo bench -p tunnel-lattice-async --bench pool   # накладные расходы и конкуренция PacketPool
```

Сквозной бенчмарк пропускной способности на `iperf3` в стиле
[tun-benchmark2](https://github.com/tun-rs/tun-benchmark2) в работе. Он
измеряет Tunnel Lattice и «голый» `tun-rs` рядом, на одной машине. Таблица
результатов появится здесь.

## 📦 Установка

```toml
[dependencies]
# Синхронный API, без async-рантайма
tunnel-lattice = "0.4"

# Асинхронный поток пакетов на Tokio (многопоточный рантайм)
tunnel-lattice = { version = "0.4", features = ["tokio"] }

# Асинхронный поток пакетов на async-io (smol, async-std, ...)
tunnel-lattice = { version = "0.4", features = ["async-io"] }
```

`tokio` и `async-io` взаимоисключающие.

## 🎓 Быстрый старт

### Синхронный TUN

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Result, Tunnel};

fn main() -> Result<()> {
    let tunnel = Tunnel::connect();
    let device = tunnel.open(DeviceConfig::new(DeviceKind::Tun).with_mtu(1500))?;
    let mut buf = vec![0u8; device.snapshot()?.recv_buffer_len()];
    let len = device.recv(&mut buf)?;
    println!("{len} bytes");
    Ok(())
}
```

`recv_buffer_len()` — это MTU для TUN и MTU + 18 для TAP (заголовок
Ethernet и один VLAN-тег); такого буфера хватает на один пакет при текущем
MTU, кроме TAP-кадра с двумя тегами (QinQ). Пакет, который не помещается,
никогда не обрезается: он отбрасывается, а `recv` возвращает
`Error::BufferTooSmall`.

### Асинхронный поток пакетов (Tokio)

```rust,ignore
use futures::StreamExt;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

#[tokio::main]
async fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    let mut packets = device.packet_stream(device.snapshot()?.recv_buffer_len())?;
    while let Some(packet) = packets.next().await {
        let packet = packet?; // представление слота пула; drop возвращает слот
        println!("{} bytes", packet.len());
    }
    Ok(())
}
```

Когда устройство исчезает, поток отдаёт одну ошибку и завершается; на
слишком большой пакет он отдаёт `BufferTooSmall` и продолжает работу.

В асинхронном коде отправляйте через `send_async(&packet).await`, а не
блокирующий `send` (он паникует внутри задачи Tokio). `send_async`
принимает полученный `PacketBuf` без копирования и возвращает те же ошибки,
что и `send`. Отброшенный `send_async` никогда не отправляет часть пакета;
на Windows неизвестно, ушёл ли пакет.

## 📚 Примеры

### Персистентное устройство и multi-queue (Linux)

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect()
        .open(DeviceConfig::new(DeviceKind::Tun).with_name("tl0").with_multi_queue(true))?;
    device.persist()?;                             // переживёт завершение процесса
    let second_queue = device.additional_queue()?; // для другого потока
    Ok(())
}
```

### Назначение адреса через net-lattice

Tunnel Lattice создаёт интерфейс, `net-lattice` его настраивает. У крейтов
намеренно нет общих идентификаторов объектов: `tunnel_lattice::DeviceId` и
`net_lattice::InterfaceId` — разные типы-обёртки с фантомным параметром,
даже если их нативный индекс случайно совпадает, так что перепутать их
местами не даст компилятор. Связывайте их через назначенное ОС имя
интерфейса — единственное поле, которое обе стороны отдают в одном виде:

```rust,no_run
use net_lattice::Lattice;
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let device = Tunnel::connect().open(DeviceConfig::new(DeviceKind::Tun))?;
    let snapshot = device.snapshot()?; // snapshot.name, например "tun0"

    let interface = Lattice::connect()?
        .interfaces()?
        .into_iter()
        .find(|i| i.name == snapshot.name)
        .ok_or(net_lattice::Error::NotFound)?;
    // дальше — адрес, поднятие интерфейса и маршруты через net-lattice.
    Ok(())
}
```

Если `name` в `DeviceConfig` не задано, имя выбирает бэкенд или ОС. Заданное
имя, которое платформа не может принять как есть, `open` отклоняет с
`Error::InvalidState`, а занятое имя — с `Error::AlreadyExists`; допустимые
форматы и случаи, когда `open` подключается к уже существующему устройству
(постоянному или многоочередному на Linux, адаптеру Wintun на Windows),
описаны в документации поля `DeviceConfig::name`. Фактическое имя всегда
берите из `snapshot()`/возвращённого `Device`, а не из переданного
`DeviceConfig`.

## 🔧 Настройка под платформу

### Linux

```bash
sudo modprobe tun                          # если нет /dev/net/tun
sudo setcap cap_net_admin+ep ./your-app    # или запуск через sudo
```

Отсутствующий модуль `tun` сообщается как `Error::DriverUnavailable`.

### Windows

- **TUN**: скачайте `wintun.dll` под вашу архитектуру с
  [wintun.net](https://www.wintun.net/) и положите рядом с исполняемым
  файлом или в `PATH`.
- **TAP**: установите драйвер
  [tap-windows6](https://github.com/OpenVPN/tap-windows6/releases).
  Достаточно поместить его пакет в хранилище драйверов (например,
  `pnputil /add-driver OemVista.inf` из `dist.win10.zip` релиза): `open`
  создаёт адаптер сам, заранее создавать его не нужно.
  `Tunnel::capabilities()` сообщает `Capability::TAP_DEVICES` на Windows,
  только если этот драйвер установлен; проверка не требует прав
  администратора, не создаёт адаптер и выполняется один раз за процесс.
- Запускайте от администратора. Без DLL или драйвера `open` вернёт
  `Error::DriverUnavailable`.

### macOS

- Запускайте от root.
- Устройства **TUN** — это `utun<N>`, **TAP** — пары `feth<N>` (`N` ≤
  32767); оставьте имя пустым, чтобы его выбрала ОС.

## 🤝 Сравнение

Сейчас Tunnel Lattice работает поверх `tun-rs`, поэтому здесь сравнивается,
что добавляет слой Tunnel Lattice и чего в нём пока нет.

| Возможность | Tunnel Lattice | tun-rs (бэкенд под ним) |
|-------------|----------------|-------------------------|
| **Тип ошибки** | ✅ Один типизированный `Error`, одинаковый на всех ОС | ⚠️ `std::io::Error` с платформенными кодами |
| **Устройство удалено во время `recv`** | ✅ Завершается с `Disconnected` на всех ОС и фичах | ⚠️ Ждёт вечно с Tokio на Linux и в async TAP на macOS |
| **Слишком большой пакет** | ✅ `BufferTooSmall`, без обрезки | ⚠️ Поведение зависит от платформы |
| **Async API** | ✅ `futures::Stream`, Tokio или async-io | ✅ async `recv`/`send`, Tokio или async-io |
| **Поток пакетов без аллокаций** | ✅ Встроенный `PacketPool` | ➖ Буферы на стороне вызывающего |
| **Флаги возможностей в рантайме** | ✅ | ❌ |
| **Заменяемый бэкенд** | ✅ Трейты провайдера | ❌ |
| **Аппаратный offload (TSO/GSO)** | 🚧 Запланирован на 0.6 | ✅ Linux |
| **Настройка адресов и маршрутов** | ➖ Через net-lattice | ✅ Встроена |
| **Платформы** | Linux, Windows, macOS | 11+, включая BSD, iOS, Android |

## 🛠️ Обзор API

| Элемент | Назначение |
|---------|------------|
| `Tunnel::connect()` | Подключение к бэкенду по умолчанию |
| `Tunnel::new(backend)` | Любой бэкенд, включая собственный или тестовый двойник |
| `Tunnel::capabilities` | Что поддерживает хост до открытия устройства |
| `Tunnel::open(DeviceConfig)` | Создать устройство TUN/TAP и вернуть `Handle` |
| `Handle::id` / `kind` | Идентификатор и тип, зафиксированные при открытии (без нативного вызова) |
| `Handle::recv` / `send` | Блокирующая передача пакетов |
| `Handle::snapshot` | Текущие имя, MTU, административное состояние и MAC-адрес TAP |
| `Handle::apply(DeviceConfigPatch)` | Изменить MTU, MAC-адрес TAP (Linux, macOS) или up/down |
| `Handle::capabilities` | Что устройство поддерживает в рантайме |
| `Handle::persist` / `additional_queue` | Персистентность и multi-queue на Linux |
| `Handle::packet_stream` | Асинхронный `Stream` пакетов из пула (`tokio` / `async-io`) |
| `Handle::send_async` | Неблокирующая отправка для асинхронного кода (`tokio` / `async-io`) |

### Крейты воркспейса

| Крейт | Назначение |
| --- | --- |
| [`tunnel-lattice`](crates/tunnel-lattice/README.md) | Публичный фасад: `Tunnel`/`Handle`, выбор бэкенда через фичи |
| [`tunnel-lattice-model`](crates/tunnel-lattice-model/README.md) | Наблюдаемые и желаемые типы устройства (`Device`, `DeviceConfig`, `DeviceConfigPatch`) |
| [`tunnel-lattice-platform`](crates/tunnel-lattice-platform/README.md) | Трейты провайдера и контракт `Capability` |
| [`tunnel-lattice-core`](crates/tunnel-lattice-core/README.md) | Общие ошибки, результаты и идентификаторы |
| [`tunnel-lattice-async`](crates/tunnel-lattice-async/README.md) | Независимый от рантайма `Stream` пакетов и `PacketPool` |
| [`tunnel-lattice-backend-tunrs`](crates/tunnel-lattice-backend-tunrs/README.md) | Реализация TUN/TAP на базе `tun-rs` |

## 📖 Документация

- **Справочник API**: [docs.rs/tunnel-lattice](https://docs.rs/tunnel-lattice)
- **Архитектура**: [ARCHITECTURE.ru.md](ARCHITECTURE.ru.md)
- **Изменения**: [CHANGELOG.md](CHANGELOG.md)
- **Поддержка и безопасность**: [SUPPORT.md](SUPPORT.md), [SECURITY.md](SECURITY.md)

## 🐛 Решение проблем

<details>
<summary><b><code>PermissionDenied</code> при открытии устройства</b></summary>

Для создания устройства нужны `CAP_NET_ADMIN` на Linux (`sudo` или
`sudo setcap cap_net_admin+ep ./your-app`), права администратора на Windows
или root на macOS.
</details>

<details>
<summary><b><code>DriverUnavailable</code> на Windows или Linux</b></summary>

На Windows нет `wintun.dll` (TUN) или драйвера tap-windows6 (TAP); см.
[настройку Windows](#windows). На Linux не загружен модуль `tun` или нет
`/dev/net/tun`; выполните `sudo modprobe tun`.
</details>

<details>
<summary><b><code>recv</code> зависает с фичей <code>tokio</code></b></summary>

С `tokio` каждому вызову устройства нужен **многопоточный** рантайм Tokio,
в который вошёл вызывающий поток (вариант `#[tokio::main]` по умолчанию).
Рантайм `current_thread` никогда не обслуживает I/O устройства для
блокирующих `recv`/`send`. В асинхронном коде используйте вместо них
`packet_stream` и `send_async`; `send_async` работает и на рантайме
`current_thread`. Если блокирующие вызовы нужны на однопоточном рантайме,
используйте `async-io`.
</details>

<details>
<summary><b><code>send</code> паникует внутри задачи Tokio</b></summary>

"Cannot start a runtime from within a runtime": блокирующие `send`/`recv`
блокируются на рантайме, а Tokio запрещает это внутри асинхронного кода.
Используйте там `send_async(&packet).await` и `packet_stream`.
</details>

<details>
<summary><b><code>InvalidState</code> для имени устройства</b></summary>

`open` отклоняет имя, которое не может выдать ровно как запрошено: длиннее
15 байт или с `%` на Linux, всё, кроме `utun<N>` / `feth<N>`, на macOS.
Оставьте имя пустым, чтобы его выбрала ОС.
</details>

## 🌐 Экосистема Lattice

| Крейт | Назначение |
| --- | --- |
| [net-lattice](https://github.com/F000NKKK/net-lattice) | Инспекция и настройка сетевого стека ОС (маршруты, DNS, интерфейсы) |
| [tunnel-lattice](https://github.com/F000NKKK/tunnel-lattice) | TUN/TAP туннельные интерфейсы |
| [dns-lattice](https://github.com/F000NKKK/dns-lattice) | Программируемый DNS control plane |
| [flow-lattice](https://github.com/F000NKKK/flow-lattice) | Компилятор политик: правила в платформенно-нейтральные сетевые планы |
| [sdk-lattice](https://github.com/F000NKKK/sdk-lattice) | Прикладной SDK, объединяющий крейты выше |

## 🙏 Участие в разработке

Мы рады вкладу; см. [CONTRIBUTING.md](CONTRIBUTING.md). На этой стадии
ценнее всего обратная связь по архитектуре и форме API в
[ARCHITECTURE.ru.md](ARCHITECTURE.ru.md).

```bash
git clone https://github.com/F000NKKK/tunnel-lattice.git
cd tunnel-lattice
cargo test --workspace                                      # тесты без привилегий
sudo -E cargo test -p tunnel-lattice-backend-tunrs -- --ignored   # на реальных устройствах
sudo -E cargo test -p tunnel-lattice --lib --features tokio -- --ignored   # фасад, на реальных устройствах
```

## 📄 Лицензия

Распространяется под [Mozilla Public License 2.0](LICENSE).

## 🌟 Благодарности

- [`tun-rs`](https://github.com/tun-rs/tun-rs) — кроссплатформенная
  TUN/TAP-библиотека, на которой сейчас работает Tunnel Lattice
- [Wintun](https://www.wintun.net/) и асинхронная экосистема Rust (Tokio,
  async-io)

---

<div align="center">

**[⬆ Наверх](#top)**

Часть сетевого стека Lattice

</div>
