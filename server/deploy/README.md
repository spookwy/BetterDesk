# Развёртывание сигналинга на VPS

Инструкция для Oracle Cloud Always Free; на любом другом VPS отличается
только созданием машины.

**Что делает этот сервер и чего не делает.** Он помнит, какой ID у
какого адреса, и сводит стороны при подключении. Через него **не идёт
ни видео, ни звук, ни ввод** (CLAUDE.md §3.1) — только несколько
сообщений на установление сессии. Поэтому нагрузки на него нет, а
трафик измеряется килобайтами за сеанс.

**Зачем он вообще нужен, если есть hole punching.** Компьютер за NAT
не знает своего внешнего адреса — видит только `192.168.x.x`. Узнать
настоящий можно единственным способом: спросить кого-то снаружи. Плюс
адрес одной стороны надо передать другой ДО того, как между ними
появилась связь. Ни одна из двух машин этого сделать не может.

---

## Шаг 1. Создать машину

В консоли Oracle Cloud: **Compute → Instances → Create instance**.

| Поле | Значение | Почему |
|---|---|---|
| Image | **Ubuntu 22.04** | ARM-образ Oracle Linux тоже подойдёт, но под Ubuntu больше готовых ответов при неполадках |
| Shape | **VM.Standard.A1.Flex**, 1 OCPU, 6 ГБ | ARM Ampere — то, что бессрочно бесплатно. Сигналингу хватит и 1 ГБ, но брать меньше бесплатного лимита незачем |
| SSH keys | **Save private key** | Скачать `.key` файл — без него на машину не зайти |

**Важно про Shape.** Если ARM-мощностей нет в наличии («Out of
capacity»), это обычное дело для Always Free — пробовать другой регион
или повторять позже. Альтернатива: **VM.Standard.E2.1.Micro** (AMD),
он тоже бесплатный и для сигналинга достаточен.

Записать **Public IP address** созданной машины.

---

## Шаг 2. Открыть порт 9000 — ДВА РАЗА

Это место, где спотыкаются все: в Oracle Cloud файрвол **двойной**, и
открыть надо оба. Открыв только один, вы получите машину, которая
пингуется, но не отвечает на 9000 — и искать причину будете долго.

### 2.1. Облачный файрвол (в консоли)

**Networking → Virtual Cloud Networks → ваша VCN → Security Lists →
Default Security List → Add Ingress Rules:**

| Поле | Значение |
|---|---|
| Source CIDR | `0.0.0.0/0` |
| IP Protocol | TCP |
| Destination Port Range | `9000` |

Протокол именно **TCP**: сигналинг — это WebSocket поверх HTTP.
(Видео идёт по UDP напрямую между машинами и через сервер не
проходит, поэтому UDP здесь открывать не нужно.)

### 2.2. Файрвол внутри машины

Ubuntu на Oracle приезжает с правилами iptables, которые блокируют
всё, кроме SSH. Команды — в шаге 4.

---

## Шаг 3. Зайти на машину

```powershell
# Windows: ключ должен быть доступен только вам, иначе ssh откажется
icacls "путь\к\ssh-key.key" /inheritance:r /grant:r "$env:USERNAME:R"

ssh -i "путь\к\ssh-key.key" ubuntu@ВАШ_IP
```

Пользователь — `ubuntu` (для Ubuntu-образа) или `opc` (Oracle Linux).

---

## Шаг 4. Установить всё одной пачкой

Скопировать целиком в SSH-сессию:

```bash
# Rust — нужен только на время сборки.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# Git и компилятор C (нужен линкеру).
sudo apt update && sudo apt install -y git build-essential

# Исходники. Репозиторий приватный, поэтому нужен токен с правом
# чтения: https://github.com/settings/tokens
git clone https://github.com/spookwy/BetterDesk.git
cd BetterDesk/server

# Сборка. На 1 OCPU занимает 3-7 минут.
cargo build --release -p bd-signaling

# Установка.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin betterdesk
sudo mkdir -p /opt/betterdesk
sudo cp target/release/bd-signaling /opt/betterdesk/
sudo cp deploy/bd-signaling.service /etc/systemd/system/

sudo systemctl daemon-reload
sudo systemctl enable --now bd-signaling

# Файрвол внутри машины (вторая половина шага 2).
sudo iptables -I INPUT 6 -m state --state NEW -p tcp --dport 9000 -j ACCEPT
sudo netfilter-persistent save
```

---

## Шаг 5. Проверить

**На сервере:**

```bash
systemctl status bd-signaling      # должно быть active (running)
curl http://localhost:9000/health  # должно ответить ok
```

**С вашего компьютера** — это и есть настоящая проверка, потому что
она идёт через оба файрвола:

```powershell
curl http://ВАШ_IP:9000/health
```

Ответ `ok` означает, что сервер работает и достижим извне.

**Если `ok` на сервере, но не снаружи** — не открыт один из двух
файрволов (шаг 2). Чаще забывают облачный.

---

## Шаг 6. Вписать адрес в программу

На **вашем** компьютере, в [crates/bd-core/src/signaling.rs](../../crates/bd-core/src/signaling.rs):

```rust
pub const DEFAULT_SIGNALING: &str = "ws://ВАШ_IP:9000/ws";
```

Пересобрать, закоммитить, отправить. После этого `bd-host.exe`
запускается двойным щелчком и печатает ID, а клиенту достаточно
девяти цифр — ни адресов, ни флагов.

---

## Обновление сервера

```bash
cd ~/BetterDesk && git pull
cd server && cargo build --release -p bd-signaling
sudo systemctl stop bd-signaling
sudo cp target/release/bd-signaling /opt/betterdesk/
sudo systemctl start bd-signaling
```

## Смотреть журнал

```bash
journalctl -u bd-signaling -f      # в реальном времени
journalctl -u bd-signaling -n 50   # последние 50 строк
```

В журнале видно регистрацию хостов и сведение сторон — по нему сразу
понятно, дошла ли до сервера каждая из двух машин.

---

## Чего здесь намеренно нет

**TLS (`wss://`).** Сейчас сигналинг ходит открытым текстом, и
провайдер видит, кто с кем соединяется. Содержимое сессии это не
раскрывает — видео шифруется QUIC и идёт мимо сервера, — но
**подменить сервер и подсунуть чужой адрес возможно**. Защита от
этого — пиннинг ключей устройств (§7.3, этап 5), а не TLS на
сигналинге: сервер и после TLS остаётся недоверенным.

Записано как осознанный компромисс этапа 4, а не как недосмотр.

**Домен.** IP в константе работает, но при смене машины придётся
пересобирать клиентов. Домен решает это, стоит ~$10/год и не нужен,
пока пользователей двое.
