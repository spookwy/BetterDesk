//! Запуск и остановка сессии из оболочки.
//!
//! # Почему дочерний процесс, а не всё в одном
//!
//! Соблазн велик: пайплайн — это библиотеки, их можно позвать прямо
//! отсюда. Но §6.1 требует, чтобы окно сессии было нативным Win32 с
//! собственным D3D11-swapchain и своим циклом сообщений, а Tauri про
//! него не знал вовсе. Держать такое окно внутри процесса оболочки
//! означает два цикла сообщений в одном приложении — источник
//! залипаний, которые потом ищут неделями.
//!
//! Отдельный процесс даёт три вещи бесплатно:
//!
//! - **падение сессии не роняет оболочку.** Захват экрана и декодер
//!   живут на чужом железе с чужими драйверами; проб на этом проекте
//!   было достаточно, чтобы не считать их непадающими;
//! - **окно сессии остаётся ровно тем, что отлажено** — `bd-host` и
//!   `bd-client` не переписываются под оболочку и продолжают
//!   работать из командной строки;
//! - **всё, что печатает сессия, уже человеческое.** Сообщения об
//!   отказах писались для человека за консолью (находки 47, 48, 61),
//!   и их можно показывать как есть.
//!
//! # Чего этот модуль не делает
//!
//! Не разбирает вывод сессии, чтобы что-то по нему решать. Он
//! показывает строки человеку и следит за одним: жив процесс или
//! нет. Разбор чужого вывода регулярками — это связь через формат,
//! который никто не обещал держать стабильным.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

/// Сколько строк вывода сессии держать.
///
/// Хватает, чтобы увидеть причину отказа целиком: самое длинное
/// объяснение в `bd-host` — десяток строк про недоступный энкодер.
/// Больше держать незачем — это окно диагностики, а не журнал.
const LOG_LINES: usize = 200;

/// Роль в сессии.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    /// Отдаём свой экран.
    Host,
    /// Смотрим чужой.
    Client,
}

/// Настройки запуска, пришедшие с экрана.
///
/// Имена полей — camelCase, как их шлёт JavaScript. Без `rename_all`
/// `peerId` со страницы не лёг бы в `peer_id`, и поле осталось бы
/// пустым **молча**: `#[serde(default)]` подставил бы пустую строку,
/// клиент запустился бы без ID и отказался бы уже там. Ошибка была бы
/// не в том месте, где причина.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LaunchOptions {
    /// К кому подключаться — девять цифр. Только для клиента.
    #[serde(default)]
    pub peer_id: String,
    /// Пароль сессии. Только для клиента.
    #[serde(default)]
    pub password: String,
    /// Пресет качества: low / medium / high.
    #[serde(default)]
    pub quality: String,
    /// Включить защиту от потерь.
    #[serde(default)]
    pub fec: bool,
    /// Разрешить второй стороне управлять этой машиной.
    ///
    /// Только для хоста и **по умолчанию выключено**: до этапа 5 нет
    /// аутентификации устройств, и отдавать управление молча нельзя.
    #[serde(default)]
    pub allow_input: bool,
}

/// Что показывать на экране про текущую сессию.
///
/// camelCase по той же причине, что у `LaunchOptions`: страница
/// читает `exitNote`, и `exit_note` остался бы для неё `undefined` —
/// без ошибки, просто пустое место там, где должно быть объяснение.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionStatus {
    /// Идёт ли сессия прямо сейчас.
    pub running: bool,
    /// Роль, если сессия идёт.
    pub role: Option<Role>,
    /// Последние строки вывода — их видит человек.
    pub log: Vec<String>,
    /// Чем закончилась прошлая сессия, если закончилась не сама.
    pub exit_note: Option<String>,
}

/// Запущенная сессия.
struct Running {
    child: Child,
    role: Role,
}

/// Состояние сессий оболочки.
pub struct Sessions {
    running: Mutex<Option<Running>>,
    log: Arc<Mutex<Vec<String>>>,
    exit_note: Arc<Mutex<Option<String>>>,
}

impl Sessions {
    pub fn new() -> Self {
        Self {
            running: Mutex::new(None),
            log: Arc::new(Mutex::new(Vec::new())),
            exit_note: Arc::new(Mutex::new(None)),
        }
    }

    /// Запустить сессию.
    ///
    /// Возвращает человеческое объяснение при отказе — оно уйдёт
    /// прямо на экран, поэтому текст здесь для человека, а не для
    /// разработчика.
    pub fn start(&self, role: Role, options: &LaunchOptions) -> Result<(), String> {
        let mut guard = self.running.lock().map_err(|_| "состояние отравлено")?;

        // Вторая сессия поверх первой заняла бы тот же порт 7000 и
        // отвалилась бы с невнятной ошибкой. Отвечаем понятно.
        if let Some(existing) = guard.as_mut() {
            match existing.child.try_wait() {
                Ok(None) => return Err("Сессия уже идёт. Сначала завершите её.".into()),
                // Процесс умер, но мы этого ещё не заметили — место
                // свободно.
                _ => *guard = None,
            }
        }

        if role == Role::Client {
            let digits: String = options
                .peer_id
                .chars()
                .filter(|c| c.is_ascii_digit())
                .collect();
            if digits.len() != 9 {
                return Err("ID — это девять цифр. Пробелы можно не убирать.".into());
            }
        }

        let exe = binary_path(role)?;
        let args = build_args(role, options);

        let mut command = Command::new(&exe);
        command
            .args(&args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        // Дочерняя консоль не нужна: её вывод мы и так читаем и
        // показываем в окне. Мелькающее чёрное окно выглядит сбоем.
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            command.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = command.spawn().map_err(|e| {
            format!(
                "Не удалось запустить {}: {e}\n\
                 Соберите продукт: cargo build --release",
                exe.display()
            )
        })?;

        // Новый прогон — новый журнал: строки прошлой сессии здесь
        // сбивали бы с толку.
        if let Ok(mut log) = self.log.lock() {
            log.clear();
        }
        if let Ok(mut note) = self.exit_note.lock() {
            *note = None;
        }

        // Оба потока вывода читаются в фоне. Не читать их нельзя:
        // когда буфер канала переполнится, дочерний процесс встанет
        // на записи — и сессия замрёт без единой ошибки.
        if let Some(stdout) = child.stdout.take() {
            spawn_reader(stdout, Arc::clone(&self.log));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_reader(stderr, Arc::clone(&self.log));
        }

        *guard = Some(Running { child, role });
        Ok(())
    }

    /// Завершить сессию.
    pub fn stop(&self) -> Result<(), String> {
        let mut guard = self.running.lock().map_err(|_| "состояние отравлено")?;
        let Some(mut running) = guard.take() else {
            return Ok(());
        };
        let _ = running.child.kill();
        let _ = running.child.wait();
        if let Ok(mut note) = self.exit_note.lock() {
            *note = Some("Сессия завершена.".into());
        }
        Ok(())
    }

    /// Текущее состояние для экрана.
    pub fn status(&self) -> SessionStatus {
        let mut running_now = false;
        let mut role = None;

        if let Ok(mut guard) = self.running.lock() {
            if let Some(existing) = guard.as_mut() {
                role = Some(existing.role);
                match existing.child.try_wait() {
                    // Ещё работает.
                    Ok(None) => running_now = true,
                    // Завершился сам — сообщаем чем.
                    Ok(Some(status)) => {
                        if let Ok(mut note) = self.exit_note.lock() {
                            *note = Some(exit_note_for(status.code()));
                        }
                        *guard = None;
                    }
                    Err(_) => *guard = None,
                }
            }
        }

        SessionStatus {
            running: running_now,
            role: if running_now { role } else { None },
            log: self.log.lock().map(|l| l.clone()).unwrap_or_default(),
            exit_note: self.exit_note.lock().ok().and_then(|n| n.clone()),
        }
    }
}

impl Default for Sessions {
    fn default() -> Self {
        Self::new()
    }
}

/// Объяснить код возврата человеку.
///
/// Ноль — штатное завершение, всё прочее означает, что сессия не
/// состоялась. Причина при этом уже напечатана в журнале самой
/// сессией: `bd-host` и `bd-client` объясняют отказы словами
/// (находки 47, 48, 61), и повторять их догадками здесь незачем.
fn exit_note_for(code: Option<i32>) -> String {
    match code {
        Some(0) => "Сессия завершена.".into(),
        Some(_) | None => "Сессия завершилась с ошибкой — причина в журнале ниже.".into(),
    }
}

/// Собрать аргументы запуска.
///
/// Вынесено отдельно ради тестов: перепутанный флаг здесь не даёт ни
/// ошибки компиляции, ни отказа при запуске — сессия просто пойдёт
/// не с теми настройками, и заметить это можно лишь по картинке.
fn build_args(role: Role, options: &LaunchOptions) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();

    if role == Role::Client {
        let digits: String = options
            .peer_id
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        args.push("--id".into());
        args.push(digits);

        if !options.password.is_empty() {
            args.push("--password".into());
            args.push(options.password.clone());
        }
    }

    // Качество и FEC задаёт ОТПРАВИТЕЛЬ, то есть хост: клиент
    // пользуется тем, что ему шлют. Отдать эти ручки клиенту значило
    // бы завести настройку, которая ни на что не влияет.
    if role == Role::Host {
        if matches!(options.quality.as_str(), "low" | "medium" | "high") {
            args.push("--quality".into());
            args.push(options.quality.clone());
        }
        if options.fec {
            args.push("--fec".into());
        }
        if options.allow_input {
            args.push("--inject-input".into());
        }
    }

    args
}

/// Где лежит бинарь роли.
///
/// Рядом с оболочкой, а не по PATH: собранный продукт кладётся в
/// один каталог, и искать его в системе означало бы запустить чужую
/// версию, если она там окажется.
fn binary_path(role: Role) -> Result<std::path::PathBuf, String> {
    // Встроенный бинарь — первым.
    //
    // Раздаваемая сборка несёт сессию внутри себя (§7.1: один файл, а
    // не папка из трёх), и распакованная копия заведомо той же версии,
    // что оболочка. Файл рядом может оказаться чужим и старым —
    // ровно та ловушка, на которой стоял сигналинг на VPS, где
    // служба месяцами запускала бинарь, не обновлявшийся вместе с
    // кодом.
    let embedded_kind = match role {
        Role::Host => crate::embedded::Binary::Host,
        Role::Client => crate::embedded::Binary::Client,
    };
    if crate::embedded::is_embedded() {
        match crate::embedded::ensure_unpacked(embedded_kind) {
            Ok(path) => return Ok(path),
            // Распаковка не удалась — не повод сдаваться: рядом
            // может лежать рабочий файл. Но молчать нельзя, иначе
            // причина потеряется.
            Err(e) => tracing::warn!(%e, "не удалось распаковать встроенный бинарь"),
        }
    }

    let name = match role {
        Role::Host => "bd-host",
        Role::Client => "bd-client",
    };
    let exe_name = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };

    let here = std::env::current_exe()
        .map_err(|e| format!("не удалось узнать свой путь: {e}"))?
        .parent()
        .ok_or("у исполняемого файла нет каталога")?
        .to_path_buf();

    let candidate = here.join(&exe_name);
    if candidate.exists() {
        return Ok(candidate);
    }

    // Отладочный запуск: оболочка лежит в ui/src-tauri/target, а
    // продукт — в корневом target. Искать там же бессмысленно.
    let workspace = here
        .ancestors()
        .find(|p| p.join("Cargo.toml").exists() && p.join("crates").exists());
    if let Some(root) = workspace {
        for profile in ["release", "debug"] {
            let path = root.join("target").join(profile).join(&exe_name);
            if path.exists() {
                return Ok(path);
            }
        }
    }

    Err(format!(
        "Не найден {exe_name}. Соберите продукт:\n\
         cargo build --release -p {name}"
    ))
}

/// Читать вывод процесса в журнал.
fn spawn_reader<R: std::io::Read + Send + 'static>(stream: R, log: Arc<Mutex<Vec<String>>>) {
    std::thread::spawn(move || {
        let reader = BufReader::new(stream);
        for line in reader.lines() {
            let Ok(line) = line else { break };
            // Строки `tracing` человеку на экране не нужны: там
            // временные метки и имена модулей. Он смотрит на то, что
            // сессия говорит ему словами.
            if line.contains("INFO ") || line.contains("DEBUG ") || line.contains("TRACE ") {
                continue;
            }
            let trimmed = line.trim_end().to_string();
            if trimmed.is_empty() {
                continue;
            }
            if let Ok(mut log) = log.lock() {
                log.push(trimmed);
                if log.len() > LOG_LINES {
                    log.remove(0);
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> LaunchOptions {
        LaunchOptions {
            peer_id: String::new(),
            password: String::new(),
            quality: String::new(),
            fec: false,
            allow_input: false,
        }
    }

    #[test]
    fn client_args_carry_id_and_password() {
        let mut o = opts();
        // Пробелы как на экране хоста: человек копирует «955 235 149».
        o.peer_id = "955 235 149".into();
        o.password = "301695".into();

        let args = build_args(Role::Client, &o);
        assert_eq!(
            args,
            vec!["--id", "955235149", "--password", "301695"],
            "пробелы обязаны отфильтроваться: человек копирует ID как показано"
        );
    }

    #[test]
    fn client_without_password_does_not_pass_empty_flag() {
        // Пустой `--password` означал бы «пароль пустой», а не «его
        // не задали»: клиент тогда не стал бы спрашивать.
        let mut o = opts();
        o.peer_id = "955235149".into();
        let args = build_args(Role::Client, &o);
        assert!(!args.contains(&"--password".to_string()));
    }

    #[test]
    fn quality_and_fec_go_to_host_only() {
        let mut o = opts();
        o.quality = "low".into();
        o.fec = true;
        o.peer_id = "955235149".into();

        let host = build_args(Role::Host, &o);
        assert!(host.contains(&"--quality".to_string()));
        assert!(host.contains(&"--fec".to_string()));

        // Клиенту эти ручки не нужны: избыточность и битрейт задаёт
        // отправитель, и настройка на клиенте ни на что не влияла бы.
        let client = build_args(Role::Client, &o);
        assert!(!client.contains(&"--quality".to_string()));
        assert!(!client.contains(&"--fec".to_string()));
    }

    #[test]
    fn input_is_off_unless_asked() {
        let o = opts();
        assert!(!build_args(Role::Host, &o).contains(&"--inject-input".to_string()));

        let mut allowed = opts();
        allowed.allow_input = true;
        assert!(build_args(Role::Host, &allowed).contains(&"--inject-input".to_string()));
    }

    #[test]
    fn garbage_quality_is_ignored() {
        // Значение приходит из webview. Передать его дальше как есть
        // значило бы отдать чужой строке место в командной строке.
        let mut o = opts();
        o.quality = "--inject-input".into();
        let args = build_args(Role::Host, &o);
        assert!(!args.contains(&"--inject-input".to_string()));
        assert!(!args.contains(&"--quality".to_string()));
    }

    #[test]
    fn exit_note_distinguishes_success_from_failure() {
        assert!(exit_note_for(Some(0)).contains("завершена"));
        assert!(exit_note_for(Some(1)).contains("ошибкой"));
        assert!(exit_note_for(None).contains("ошибкой"));
    }

    #[test]
    fn wire_names_match_what_the_screen_uses() {
        // Экран шлёт camelCase и читает camelCase. Расхождение здесь
        // не даёт ни ошибки компиляции, ни отказа: поле молча
        // становится пустым, и клиент запускается без ID.
        //
        // Тот же класс, что находка 57б — вёрстка и бэкенд по
        // отдельности верны, а вместе не работают.
        let json = r#"{"peerId":"955235149","password":"301695",
                       "quality":"low","fec":true,"allowInput":false}"#;
        let parsed: LaunchOptions = serde_json::from_str(json).expect("разбор");
        assert_eq!(parsed.peer_id, "955235149");
        assert_eq!(parsed.password, "301695");
        assert!(parsed.fec);

        let status = SessionStatus {
            running: true,
            role: Some(Role::Host),
            log: vec!["строка".into()],
            exit_note: Some("почему".into()),
        };
        let out = serde_json::to_string(&status).expect("сериализация");
        assert!(out.contains("exitNote"), "экран читает exitNote: {out}");
        assert!(out.contains("\"host\""), "роль в нижнем регистре: {out}");
    }
}
