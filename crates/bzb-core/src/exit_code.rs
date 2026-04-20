use pueue_lib::task::TaskResult;

/// Maps a pueue `TaskResult` to the process exit code busybee should use
/// when relaying the task's completion to the caller.
///
/// Conventions (matches §7 of the design spec):
/// - `Success` → 0
/// - `Failed(n)` → n
/// - `Killed` → 130 (SIGINT convention)
/// - `FailedToSpawn(_)` → 127 ("command not found" convention)
/// - `Errored` / `DependencyFailed` → 1
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
