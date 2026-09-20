# Архитектура Tunnel Lattice

Tunnel Lattice создаёт и настраивает виртуальные интерфейсы TUN/TAP и
передаёт через них пакеты. Назначение IP-адресов на созданный интерфейс —
задача `net-lattice` (соседний крейт экосистемы Lattice), а не этого крейта.

## Карта крейтов

```text
tunnel-lattice-core             ошибки, ID — без зависимости от ОС
tunnel-lattice-model             DeviceKind/DeviceConfig/Device/DeviceConfigPatch — без зависимости от ОС
tunnel-lattice-platform          обобщённые provider-трейты + Capability — зависит только от core, никогда от model
tunnel-lattice-backend-tunrs     реализация platform-трейтов поверх tun-rs
tunnel-lattice-async             адаптер futures::Stream поверх синхронного PacketIo-устройства
tunnel-lattice                   фасад: связывает platform-трейты с типами model, выбирает backend
```

Направление зависимостей: `core` ← `model`, `core` ← `platform`; `model` и
`platform` никогда не зависят друг от друга напрямую — именно
`tunnel-lattice-backend-tunrs` и фасад `tunnel-lattice` связывают обобщённые
associated types `platform` с конкретными типами `model`. Это то же правило,
что и «`net-lattice-platform` никогда не зависит от `net-lattice-model`» в
соседней экосистеме, и по той же причине: backend или фасад решают, какие
конкретные типы удовлетворяют контракту, а не сам контракт.

## Почему это отличается от net-lattice

`net-lattice` инспектирует и изменяет объекты ОС (маршруты, интерфейсы,
настройки DNS), существующие независимо от вызывающего процесса — маршрут
существует вне зависимости от того, следит ли за ним какой-либо процесс,
поэтому `net-lattice-platform` разделяет чтение (`RouteProvider`) и запись
(`RouteMutator`) по доменам, а фасад держит одно постоянное соединение
(`Lattice<Backend>`), используемое для множества вызовов по ID объекта.

У TUN/TAP-устройства нет такого независимого существования: оно существует
только потому, что его создал текущий процесс, и только этот процесс держит
единственный дескриптор. Поэтому:

- нет отдельного шага «подключения» перед открытием устройства — открытие
  и есть привилегированная операция (`DeviceProvider::open`, а не
  `Lattice::connect` с последующими вызовами по ID);
- возвращённый дескриптор устройства сам является целью каждой последующей
  операции (`PacketIo`, `DeviceObserver`, `DeviceMutator`), а не ID,
  передаваемым в общий объект соединения;
- нет доменного чтения «список всех устройств», как у `RouteProvider` для
  маршрутов — backend, способный перечислить уже существующие постоянные
  устройства, предоставляет это как собственный inherent API, за флагом
  `Capability::PERSISTENT_DEVICES`, а не как обязательный метод трейта.

## План замены backend'а

`tunnel-lattice-backend-tunrs` оборачивает крейт `tun-rs`, который сам по
себе кроссплатформенный (Linux, Windows, macOS, BSD, iOS, Android) — в
отличие от взаимоисключающих backend'ов net-lattice для каждой ОС
(`net-lattice-backend-linux`/`-windows`/`-darwin`, каждый за `target_os`
cfg), Tunnel Lattice начинает с одного общего крейта backend'а, поскольку
пока нет платформенно-специфичного Rust-кода, который нужно изолировать —
`tun-rs` уже сделал эту работу внутри себя.

Воркспейс намеренно устроен так, чтобы `tun-rs` можно было убрать позже, не
трогая публичный API `tunnel-lattice-platform` или фасада `tunnel-lattice`:

- трейты `tunnel-lattice-platform` (`DeviceProvider`, `PacketIo`,
  `DeviceObserver`, `DeviceMutator`, `AsyncPacketIo`) не называют ни одного
  типа `tun-rs` — они обобщены через associated types, которым сегодня
  удовлетворяют `tunnel-lattice-backend-tunrs::TunRsBackend`/`TunRsDevice`;
- фасад выбирает backend через Cargo-**фичу** (`tun-rs`, включена по
  умолчанию), а не через `target_os` cfg. Будущий написанный вручную
  backend для конкретной ОС (например, `tunnel-lattice-backend-linux`
  напрямую поверх `/dev/net/tun` + Netlink, по аналогии с использованием
  Netlink в `net-lattice-backend-linux`) поставлялся бы отдельным крейтом
  со своей фичой фасада, выбираемой *наряду* с `tun-rs`, а не только взамен
  целиком — вызывающий сможет подключить `tunnel-lattice` с
  `default-features = false, features = ["linux-native"]`, когда это
  появится;
- когда backend для конкретной ОС покроет все платформы, которые сегодня
  покрывает `tun-rs`, отказ от фичи `tun-rs` (и самой зависимости) станет
  изменением только в крейте backend'а — ничего в `tunnel-lattice-model`
  или `tunnel-lattice-platform` переносить не придётся.

Backend для конкретной ОС пока не существует — этот раздел фиксирует
намеченную форму, а не выполненную работу.

## Модель ошибок

Повторяет `net-lattice-core::Error`: один `#[non_exhaustive]` enum
(`PermissionDenied`, `NotFound`, `AlreadyExists`, `Unsupported`,
`InvalidState`, `Disconnected`, `Platform(PlatformErrorCode)`),
возвращаемый каждым методом provider-трейта вместо «сырой» ошибки ОС.
Backend отображает свою нативную ошибку (сейчас — `std::io::Error` для
`tun-rs`) в эту форму на границе; вызывающий код никогда не сопоставляет
паттерн напрямую с `errno`/`DWORD`.

## Зафиксированный публичный API

Tunnel Lattice ещё не публиковал релиз (см. `index.md`, `SUPPORT.md`).
Ничего в этом воркспейсе не заморожено по API — любой тип, трейт и
Cargo-фича, описанные здесь, могут измениться до выхода `0.1.0`.

## Асинхронный дизайн

Фича `async` крейта `tunnel-lattice-platform` добавляет `AsyncPacketIo` —
трейт, возвращающий `impl Future`, который backend реализует при наличии
настоящего неблокирующего ввода-вывода (файловый дескриптор,
зарегистрированный в асинхронном рантайме, overlapped I/O, ...).
`tunnel-lattice-backend-tunrs` реализует его через клонированный дескриптор
`tun_rs::AsyncDevice` и сообщает `Capability::NATIVE_ASYNC`.

`tunnel-lattice-async` даёт запасной вариант для backend'а без нативного
асинхронного пути: `from_device` запускает один блокирующий рабочий поток
на устройство, перекладывающий `PacketIo::recv` в `futures::Stream`. Ни
один из этих крейтов по умолчанию не зависит от Tokio, async-std или smol
напрямую — фича `async` крейта `tunnel-lattice` опциональна, поэтому
вызывающий код, не включивший её, вообще не тянет асинхронный рантайм.
Известное ограничение: адаптер на потоках не может принудительно отменить
рабочий поток, заблокированный внутри `recv` при отсутствии новых пакетов —
см. rustdoc `tunnel-lattice-async`.

## Платформенные и привилегированные особенности

Создание TUN/TAP-устройства обычно требует `CAP_NET_ADMIN` на Linux,
Administrator на Windows или root на macOS/BSD. Флаги `Capability` описывают
реализованные поверхности, а не гарантию того, что текущий процесс
авторизован — вызывающему коду всё равно стоит обрабатывать ошибку
доступа от `DeviceProvider::open`, даже если возможность заявлена как
доступная.
