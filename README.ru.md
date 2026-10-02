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

> **Статус:** опубликована `0.5.0`, идёт активная разработка. До `1.0` API
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
- ✅ **Пакетная отправка**: `send_batch` (и `send_batch_async`) отправляет
  префикс списка пакетов за один вызов на любой ОС; `Ok(n)` говорит,
  сколько ушло

### Платформенные возможности
- 🐧 **Персистентность на Linux**: устройство переживает завершение процесса,
  к нему можно позже подключиться по имени и снять персистентность
- 🔀 **Multi-queue на Linux**: независимые очереди ядра на одном устройстве
- 📦 **Segmentation offload для TUN на Linux**: включается через
  `with_offload(true)`; ядро передаёт TCP/UDP-трафик суперпакетами до
  64 КиБ, а `recv` по-прежнему возвращает один пакет за вызов, и
  `send_batch` склеивает пакеты одного потока
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

| Платформа   | TUN | TAP | Sync | Tokio | async-io | Привилегии | Драйвер |
|-------------|:---:|:---:|:----:|:-----:|:--------:|------------|---------|
| **Linux**   | ✅  | ✅  | ✅   | ✅    | ✅       | `CAP_NET_ADMIN` (root или `setcap`) | Модуль ядра `tun` (`/dev/net/tun`) для TUN и TAP |
| **Windows** | ✅  | ✅  | ✅   | ✅    | ✅       | Администратор | TUN: Wintun, `wintun.dll` рядом с исполняемым файлом или в `PATH`; TAP: драйвер tap-windows6 (`tap0901`) в хранилище драйверов |
| **macOS**   | ✅  | ✅  | ✅   | ✅    | ✅       | root | Не нужен: TUN — это `utun`, TAP — пара `feth`, оба встроены в систему |

✅ проверено в CI на реальных устройствах. Привилегированные задания на
`ubuntu-latest`, `windows-latest` и `macos-latest` открывают, используют и
удаляют TUN- и TAP-устройства во всех наборах фич (по умолчанию, `tokio`,
`async-io`). На Windows задание сначала скачивает Wintun 0.14.1 и помещает
пакет драйвера tap-windows6 9.27.0 в хранилище драйверов, не создавая
адаптер. Отдельное задание на Linux проверяет, что персистентное устройство
переживает свой процесс, а второй процесс подключается к нему и снимает
персистентность.

На Linux также есть персистентные устройства, multi-queue и segmentation
offload для TUN. Перечислены
только платформы, которые проверяет CI: `tun-rs` работает и на других
(BSD, iOS, Android, ...), но Tunnel Lattice их не заявляет.

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

#### Пропускная способность форвардера (iperf3, Linux)

Бенчмарк [`bench/forwarder`](bench/forwarder/README.md) повторяет методику
[tun-benchmark2](https://github.com/tun-rs/tun-benchmark2) из tun-rs:
форвардер копирует пакеты между двумя TUN-устройствами, а `iperf3` измеряет
пропускную способность TCP через них. Каждый прогон измеряет «голый»
`tun-rs` (sync, Tokio и async-io) рядом с Tunnel Lattice на одной машине, и
каждая строка Tunnel Lattice сравнивается со строкой `tun-rs` над ней.
Offload не использует ни одна сторона. Числа взяты без изменений из
записанного [прогона workflow](https://github.com/F000NKKK/tunnel-lattice/actions/runs/36855227427);
переведены только подписи:

| Конфигурация | Пропускная способность (медиана, Гбит/с) | Доля от tun-rs в том же прогоне | CPU, среднее | RSS, максимум | Ретрансмиссии |
|---|---:|---:|---:|---:|---:|
| tun-rs sync | 2.87 | — | 173 % | 2.7 MB | 79 |
| tunnel-lattice sync (Handle::recv/send) | 2.73 | 94.9 % (94.4–96.6) | 173 % | 2.5 MB | 74 |
| tun-rs async (Tokio) | 2.35 | — | 179 % | 3.6 MB | 64 |
| tunnel-lattice async (Tokio, packet_stream + send_async) | 2.20 | 93.6 % (90.2–96.1) | 181 % | 3.5 MB | 57 |
| tunnel-lattice async (Tokio, один общий PacketPool) | 2.18 | 92.9 % (88.3–97.2) | 180 % | 3.5 MB | 61 |
| tun-rs async (async-io) | 3.04 | — | 187 % | 2.8 MB | 90 |
| tunnel-lattice async (async-io, packet_stream + send_async) | 2.58 | 86.2 % (80.3–92.3) | 183 % | 2.8 MB | 55 |

Записано 2026-10-01T11:26:41Z на раннере GitHub Actions ubuntu24 20260927.320.1, AMD EPYC 9V74 80-Core Processor, 4 CPU, Linux 6.17.0-1022-azure.
Код 9f5d286; tun-rs 2.8.11, tunnel-lattice 0.4.0, iperf3 3.16; rustc 1.98.1 (48a229cea 2026-09-01), RUSTFLAGS `-C target-cpu=native`.
Методика: два TUN-устройства (одно перенесено в сетевое пространство имён), соединённые форвардером; `iperf3 -t 10` по TCP с хоста на сервер в пространстве имён. Каждая строка — медиана 5 прогонов (порядок сдвигается в каждом повторе, отброшено прогревочных прогонов: 1). Пропускная способность — скорость на стороне получателя iperf3. «Доля от tun-rs в том же прогоне» — медиана (мин.–макс.) отношения к базовой строке tun-rs над ней, вычисленного для каждого повтора отдельно, в том же повторе. CPU — время user+system процесса форвардера за секунду, с выборкой 1 Гц (100 % = одно ядро); RSS — наибольший замеренный резидентный объём.
Воспроизведение: `scripts/bench-forward.sh build && sudo scripts/bench-forward.sh run && scripts/bench-forward.sh report <dir>`.
Абсолютные Гбит/с с общих или виртуальных раннеров нельзя сравнивать с числами, опубликованными для другого железа; сравнивайте долю в том же прогоне.

Чтобы воспроизвести на GitHub, запустите вручную workflow **Forwarder
benchmark** (`workflow_dispatch` в `.github/workflows/bench-forward.yml`):
он пишет таблицу в сводку задания и загружает `results.md`, `results.json`
и все сырые прогоны как артефакт. Локальный запуск требует root, `iperf3` и
Linux. Задание CI `bench-forwarder` (в `.github/workflows/ci.yml`) при
каждом push и pull request только собирает, проверяет линтером и тестирует
харнесс; сам бенчмарк оно не запускает.

**Что это показывает и чего нет.** Пока единственный бэкенд оборачивает
`tun-rs`, Tunnel Lattice в лучшем случае равен `tun-rs` за вычетом
собственных накладных расходов; столбец с долей измеряет именно их.
Диапазон в скобках — разброс между повторами только этого одного
записанного прогона; о том, насколько доля меняется между прогонами или
машинами, он ничего не говорит. Чтобы
обогнать `tun-rs`, нужны пакетный ввод-вывод и GSO/GRO-offload
(запланированы на 0.6), а затем нативные бэкенды для каждой ОС.
Собственные опубликованные числа `tun-rs` получены на другом железе, а его
главные результаты — с offload, поэтому сравнивать их с этой таблицей
нельзя.

#### Микробенчмарки

Работают на моках устройств в памяти, привилегии не нужны:

```bash
cargo bench -p tunnel-lattice-async                # пути потока против простого цикла recv
cargo bench -p tunnel-lattice-async --bench pool   # накладные расходы и конкуренция PacketPool
```

## 📦 Установка

```toml
[dependencies]
# Синхронный API, без async-рантайма
tunnel-lattice = "0.5"

# Асинхронный поток пакетов на Tokio (многопоточный рантайм)
tunnel-lattice = { version = "0.5", features = ["tokio"] }

# Асинхронный поток пакетов на async-io (smol, async-std, ...)
tunnel-lattice = { version = "0.5", features = ["async-io"] }
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

`persist` и `additional_queue` есть только на Linux, поэтому этот пример
не компилируется на Windows и macOS.

Другой процесс позже подключается к нему, открыв устройство с тем же
именем, типом и настройкой multi-queue, и может снять персистентность,
чтобы устройство исчезло вместе с последним дескриптором. `unpersist`
тоже есть только на Linux, поэтому и этот пример не компилируется на
Windows и macOS:

```rust,no_run
use tunnel_lattice::{DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let device = Tunnel::connect()
        .open(DeviceConfig::new(DeviceKind::Tun).with_name("tl0").with_multi_queue(true))?;
    device.unpersist()?; // удалится, когда будут закрыты все его дескрипторы
    Ok(())
}
```

`open` не сообщает, подключился ли он к существующему устройству или
создал новое, а несовпадение типа или настройки multi-queue завершается
ошибкой `Error::AlreadyExists`.

### Пакетная отправка и segmentation offload

`send_batch` отправляет префикс списка пакетов и возвращает, сколько
пакетов ушло. Он может отправить меньше, чем передано (неполная
отправка), поэтому вызывайте его в цикле:

```rust,no_run
use tunnel_lattice::{Capability, DeviceConfig, DeviceKind, Tunnel};

fn main() -> tunnel_lattice::Result<()> {
    let config = DeviceConfig::new(DeviceKind::Tun).with_offload(true); // запрос
    let device = Tunnel::connect().open(config)?;
    let offload = device.capabilities().contains(Capability::SEGMENTATION_OFFLOAD);
    println!("segmentation offload используется: {offload}");

    let packets: Vec<&[u8]> = Vec::new(); // целые IP-пакеты, например из recv
    let mut rest = &packets[..];
    while !rest.is_empty() {
        let sent = device.send_batch(rest)?; // Ok(n): ушли packets[..n]
        rest = &rest[sent..];
    }
    Ok(())
}
```

- **Контракт префикса.** `Ok(n)` значит, что первые `n` пакетов отправлены
  целиком и по порядку, а остальные не тронуты. `Err` значит, что ничего
  не отправлено, и ошибка относится к первому пакету. Сбой после того, как
  ушёл хотя бы один пакет, возвращается как `Ok(k)`; следующий вызов,
  начиная с этого пакета, вернёт ошибку. Пустой список даёт `Ok(0)`.
- **Отмена.** Отброшенный future `send_batch_async` успел отправить
  неизвестный префикс списка: каждый пакет целиком и не более одного раза,
  и никогда — пакет после неотправленного.
- **Offload — только для TUN на Linux, включается явно и является
  запросом, а не гарантией.** На Windows, macOS и для TAP он молча (без
  ошибки) игнорируется. Используется ли он очередью, говорит
  `Capability::SEGMENTATION_OFFLOAD` у открытого дескриптора (и никогда —
  `Tunnel::capabilities()`); очередь, подключённая к существующему
  многоочередному устройству, следует формату кадров этого устройства,
  что бы ни запросил этот `open`.
- **Что меняет offload.** Ничего видимого вызывающему: `recv` и
  `packet_stream` по-прежнему отдают по одному IP-пакету, нарезанному из
  суперпакетов ядра, а асинхронный `recv`, отброшенный на середине, не
  теряет ни одного сегмента. На очереди с offload `send_batch` отправляет
  не больше 128 пакетов за вызов и склеивает соседние пакеты одного TCP-
  или UDP-потока в одну запись; если ядро отвергает склеенную запись, эти
  пакеты отправляются заново по одному. Без offload и на любой другой ОС
  `send_batch` отправляет пакеты по одному.
- **Побочный эффект на всё устройство.** Настройка offload в ядре
  принадлежит всему устройству, а не одной очереди. Открытие устройства
  без offload, в том числе обычной очереди на общем многоочередном
  устройстве с offload, выключает суперпакеты для всех его очередей, и
  при закрытии дескриптора настройка не восстанавливается. Эти очереди
  продолжают работать, по одному пакету за чтение.

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
что добавляет слой Tunnel Lattice, что он наследует от `tun-rs` и чего в
нём пока нет.

| Возможность | Tunnel Lattice | Источник | tun-rs (бэкенд под ним) |
|-------------|----------------|----------|-------------------------|
| **TUN на Linux, Windows, macOS** | ✅ | Унаследовано от `tun-rs` | ✅ |
| **TAP на Linux, Windows, macOS** | ✅ на macOS через пары `feth` | Унаследовано; Tunnel Lattice добавляет ограниченное ожидание, которое завершает async TAP `recv` на macOS | ✅ |
| **Тип ошибки** | ✅ Один типизированный `Error`, одинаковый на всех ОС | Tunnel Lattice | ⚠️ `std::io::Error` с платформенными кодами |
| **Устройство удалено во время `recv`** | ✅ Завершается с `Disconnected` на всех ОС и фичах | Tunnel Lattice | ⚠️ Ждёт вечно с Tokio на Linux и в async TAP на macOS |
| **Слишком большой пакет** | ✅ `BufferTooSmall`, без обрезки | Tunnel Lattice | ⚠️ Поведение зависит от платформы |
| **Асинхронный ввод-вывод** | ✅ `futures::Stream` и `send_async`, Tokio или async-io | Async I/O унаследован; поток — Tunnel Lattice | ✅ async `recv`/`send`, Tokio или async-io |
| **Поток пакетов без аллокаций** | ✅ Встроенный `PacketPool` | Tunnel Lattice | ➖ Буферы на стороне вызывающего |
| **Флаги возможностей в рантайме** | ✅ Для хоста и для устройства | Tunnel Lattice | ❌ |
| **Подстановка бэкенда** | ✅ `Tunnel::new(backend)` поверх трейтов провайдера | Tunnel Lattice | ❌ |
| **MAC-адрес TAP** | ✅ Задаётся при открытии; меняется на открытом устройстве на Linux и macOS | Унаследовано | ✅ |
| **Персистентные устройства (Linux)** | ✅ Сделать персистентным, подключиться по имени, снять персистентность | Установка унаследована; снятие — Tunnel Lattice | ⚠️ Только установка |
| **Multi-queue (Linux)** | ✅ `additional_queue` | Унаследовано | ✅ |
| **Пакетная отправка** | ✅ `send_batch` на любой ОС; склейка — только на очереди TUN с offload на Linux | Tunnel Lattice | ✅ Linux (`send_multiple`) |
| **GSO/GRO-offload** | ✅ TUN на Linux, явное включение; один пакет на `recv` | Tunnel Lattice (собственный разбор заголовка, нарезка и склейка) | ✅ Linux |
| **Пакетный приём** | ❌ Один пакет на `recv` | — | ✅ Linux (`recv_multiple`) |
| **Настройка адресов и маршрутов** | ➖ Через net-lattice | — | ✅ Встроена |
| **Пропускная способность** | Измеряется относительно `tun-rs` в том же прогоне; см. [Бенчмарки](#-бенчмарки) | — | Базовая линия |
| **Платформы** | Linux, Windows, macOS | — | 11+, включая BSD, iOS, Android |

## 🛠️ Обзор API

| Элемент | Назначение |
|---------|------------|
| `Tunnel::connect()` | Подключение к бэкенду по умолчанию |
| `Tunnel::new(backend)` | Любой бэкенд, включая собственный или тестовый двойник |
| `Tunnel::capabilities` | Что поддерживает хост до открытия устройства |
| `Tunnel::open(DeviceConfig)` | Создать устройство TUN/TAP и вернуть `Handle` |
| `DeviceConfig::with_offload` | Запросить segmentation offload для TUN на Linux (подсказка; ответ — `Capability::SEGMENTATION_OFFLOAD`) |
| `Handle::id` / `kind` | Идентификатор и тип, зафиксированные при открытии (без нативного вызова) |
| `Handle::recv` / `send` | Блокирующая передача пакетов |
| `Handle::send_batch` | Блокирующая отправка префикса списка пакетов; возвращает, сколько ушло |
| `Handle::snapshot` | Текущие имя, MTU, административное состояние и MAC-адрес TAP |
| `Handle::apply(DeviceConfigPatch)` | Изменить MTU, MAC-адрес TAP (Linux, macOS) или up/down |
| `Handle::capabilities` | Что устройство поддерживает в рантайме |
| `Handle::persist` / `unpersist` / `additional_queue` | Персистентность и multi-queue на Linux |
| `Handle::packet_stream` | Асинхронный `Stream` пакетов из пула (`tokio` / `async-io`) |
| `Handle::send_async` | Неблокирующая отправка для асинхронного кода (`tokio` / `async-io`) |
| `Handle::send_batch_async` | Неблокирующая пакетная отправка для асинхронного кода (`tokio` / `async-io`) |

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

С `tokio` для открытия устройства вызывающий поток должен войти в рантайм
Tokio, а блокирующим `recv`/`send` этот рантайм нужен **многопоточным**
(вариант `#[tokio::main]` по умолчанию). Рантайм `current_thread`
никогда не обслуживает I/O устройства для блокирующих `recv`/`send`. В асинхронном коде используйте вместо них
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
