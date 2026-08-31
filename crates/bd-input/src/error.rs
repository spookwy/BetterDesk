//! Ошибки захвата и инжекта ввода.

/// Результат операций ввода.
pub type Result<T> = std::result::Result<T, InputError>;

/// Ошибка ввода.
#[derive(Debug, thiserror::Error)]
pub enum InputError {
    /// Система приняла не все события.
    ///
    /// Самая частая причина — **UIPI**: процесс с меньшим уровнем
    /// целостности не может слать ввод окну с большим. Инжект в окно,
    /// запущенное от администратора, не пройдёт, пока наш процесс не
    /// поднят так же.
    ///
    /// Это не повод рвать сессию: следующее окно может принять ввод
    /// нормально. Но и молчать нельзя — при отпускании клавиш
    /// недосланное событие и есть залипание.
    #[error("система приняла {sent} из {total} событий (код {code})")]
    Blocked {
        /// Сколько событий принято.
        sent: usize,
        /// Сколько отправлено.
        total: usize,
        /// Код `GetLastError`.
        code: u32,
    },

    /// Ошибка платформенного API.
    #[error("ошибка ввода: {context} (код {code:#010x})")]
    Platform {
        /// Что пытались сделать.
        context: &'static str,
        /// Код ошибки.
        code: u32,
    },
}

impl InputError {
    /// Можно ли продолжать сессию.
    ///
    /// Блокировка ввода конкретным окном (UIPI) — штатная ситуация,
    /// а не повод отключаться: пользователь переключит окно, и ввод
    /// снова заработает.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, InputError::Blocked { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocked_is_recoverable() {
        // UIPI срабатывает при наведении на окно администратора —
        // считать это фатальным значило бы рвать сессию на ровном месте.
        let err = InputError::Blocked {
            sent: 0,
            total: 2,
            code: 5,
        };
        assert!(err.is_recoverable());
    }

    #[test]
    fn blocked_message_reports_both_counts() {
        let err = InputError::Blocked {
            sent: 1,
            total: 3,
            code: 5,
        };
        let msg = err.to_string();
        assert!(msg.contains('1') && msg.contains('3'), "текст: {msg}");
    }
}
