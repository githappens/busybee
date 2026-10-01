use pueue_lib::task::TaskResult;

/// The single place a pueue `TaskResult` becomes a process exit code.
/// `Killed` → 130 (SIGINT convention), `FailedToSpawn` → 127 (not found).
pub fn task_result_to_exit_code(result: &TaskResult) -> i32 {
    match result {
        TaskResult::Success => 0,
        TaskResult::Failed(code) => *code,
        TaskResult::Killed => 130,
        TaskResult::FailedToSpawn(_) => 127,
        TaskResult::Errored | TaskResult::DependencyFailed => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn success_is_zero() {
        assert_eq!(task_result_to_exit_code(&TaskResult::Success), 0);
    }

    #[test]
    fn failed_passes_through_code() {
        assert_eq!(task_result_to_exit_code(&TaskResult::Failed(7)), 7);
        assert_eq!(task_result_to_exit_code(&TaskResult::Failed(1)), 1);
    }

    #[test]
    fn killed_is_130() {
        assert_eq!(task_result_to_exit_code(&TaskResult::Killed), 130);
    }

    #[test]
    fn failed_to_spawn_is_127() {
        let r = TaskResult::FailedToSpawn("nope".into());
        assert_eq!(task_result_to_exit_code(&r), 127);
    }

    #[test]
    fn errored_is_one() {
        assert_eq!(task_result_to_exit_code(&TaskResult::Errored), 1);
    }

    #[test]
    fn dependency_failed_is_one() {
        assert_eq!(task_result_to_exit_code(&TaskResult::DependencyFailed), 1);
    }
}
