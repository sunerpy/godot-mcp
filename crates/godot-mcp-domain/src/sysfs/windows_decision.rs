use super::{CasOutcome, SysError};

const FILE_TYPE_UNKNOWN: u32 = 0x0000;
const FILE_TYPE_DISK: u32 = 0x0001;
const FILE_TYPE_CHAR: u32 = 0x0002;
const FILE_TYPE_PIPE: u32 = 0x0003;
const ERROR_SUCCESS: u32 = 0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HandleKind {
    Disk,
    Pipe,
    Character,
    Unknown,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum HandleKindError {
    ApiFailure(u32),
    Unsupported(u32),
}

pub(super) fn classify_handle_kind(
    file_type: u32,
    error: u32,
) -> Result<HandleKind, HandleKindError> {
    match file_type {
        FILE_TYPE_DISK => Ok(HandleKind::Disk),
        FILE_TYPE_PIPE => Ok(HandleKind::Pipe),
        FILE_TYPE_CHAR => Ok(HandleKind::Character),
        FILE_TYPE_UNKNOWN => match error {
            ERROR_SUCCESS => Ok(HandleKind::Unknown),
            error => Err(HandleKindError::ApiFailure(error)),
        },
        other => Err(HandleKindError::Unsupported(other)),
    }
}

pub(super) fn regular_file_by_kind(is_directory: bool, kind: HandleKind) -> bool {
    !is_directory && kind == HandleKind::Disk
}

pub(super) fn aliases_stdout_by_kind(
    sink_kind: HandleKind,
    stdout_kind: HandleKind,
    compare_disk_identity: impl FnOnce() -> Result<bool, SysError>,
) -> Result<bool, SysError> {
    if sink_kind != HandleKind::Disk {
        return Err(SysError::NotRegularFile);
    }
    standard_handles_alias_by_kind(sink_kind, stdout_kind, compare_disk_identity, || Ok(false))
}

pub(super) fn standard_handles_alias_by_kind(
    left_kind: HandleKind,
    right_kind: HandleKind,
    compare_disk_identity: impl FnOnce() -> Result<bool, SysError>,
    compare_kernel_object: impl FnOnce() -> Result<bool, SysError>,
) -> Result<bool, SysError> {
    match (left_kind, right_kind) {
        (HandleKind::Unknown, _) | (_, HandleKind::Unknown) => Err(SysError::Io(
            std::io::Error::other("a process standard handle has an unknown handle type"),
        )),
        (HandleKind::Disk, HandleKind::Disk) => compare_disk_identity(),
        (HandleKind::Pipe, HandleKind::Pipe) | (HandleKind::Character, HandleKind::Character) => {
            compare_kernel_object()
        }
        (HandleKind::Disk, HandleKind::Pipe | HandleKind::Character)
        | (HandleKind::Pipe | HandleKind::Character, HandleKind::Disk)
        | (HandleKind::Pipe, HandleKind::Character)
        | (HandleKind::Character, HandleKind::Pipe) => Ok(false),
    }
}

pub(super) fn cas_create_preflight(
    observed: Result<bool, SysError>,
) -> Result<Option<CasOutcome>, SysError> {
    match observed {
        Ok(true) | Err(SysError::NotRegularFile) => Ok(Some(CasOutcome::Stale)),
        Ok(false) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_handle_kind_covers_the_documented_return_space() {
        let cases = [
            (FILE_TYPE_DISK, HandleKind::Disk),
            (FILE_TYPE_PIPE, HandleKind::Pipe),
            (FILE_TYPE_CHAR, HandleKind::Character),
            (FILE_TYPE_UNKNOWN, HandleKind::Unknown),
        ];
        for (file_type, expected) in cases {
            assert_eq!(classify_handle_kind(file_type, ERROR_SUCCESS), Ok(expected));
        }
    }

    #[test]
    fn classify_handle_kind_distinguishes_api_failures_from_unsupported_values() {
        assert_eq!(
            classify_handle_kind(FILE_TYPE_UNKNOWN, 6),
            Err(HandleKindError::ApiFailure(6))
        );
        assert_eq!(
            classify_handle_kind(4, ERROR_SUCCESS),
            Err(HandleKindError::Unsupported(4))
        );
    }

    #[test]
    fn regular_file_by_kind_requires_a_non_directory_disk_handle() {
        let cases = [
            (false, HandleKind::Disk, true),
            (true, HandleKind::Disk, false),
            (false, HandleKind::Pipe, false),
            (false, HandleKind::Character, false),
            (false, HandleKind::Unknown, false),
        ];
        for (is_directory, kind, expected) in cases {
            assert_eq!(regular_file_by_kind(is_directory, kind), expected);
        }
    }

    #[test]
    fn pipe_and_character_stdout_do_not_alias_a_disk_sink() {
        for stdout_kind in [HandleKind::Pipe, HandleKind::Character] {
            let aliases = aliases_stdout_by_kind(HandleKind::Disk, stdout_kind, || {
                panic!("non-disk stdout must not trigger a file identity query")
            })
            .expect("pipe and character stdout are valid protocol handles");
            assert!(!aliases);
        }
    }

    #[test]
    fn non_disk_sinks_are_rejected() {
        for sink_kind in [HandleKind::Pipe, HandleKind::Character, HandleKind::Unknown] {
            assert!(matches!(
                aliases_stdout_by_kind(sink_kind, HandleKind::Disk, || Ok(false)),
                Err(SysError::NotRegularFile)
            ));
        }
    }

    #[test]
    fn two_disk_handles_use_file_identity() {
        assert!(
            aliases_stdout_by_kind(HandleKind::Disk, HandleKind::Disk, || Ok(true))
                .expect("disk identity comparison")
        );
    }

    #[test]
    fn unknown_stdout_fails_closed() {
        assert!(matches!(
            aliases_stdout_by_kind(HandleKind::Disk, HandleKind::Unknown, || Ok(false)),
            Err(SysError::Io(_))
        ));
    }

    #[test]
    fn two_disk_standard_handles_use_file_identity() {
        assert!(
            standard_handles_alias_by_kind(
                HandleKind::Disk,
                HandleKind::Disk,
                || Ok(true),
                || panic!("disk handles must not use kernel-object comparison"),
            )
            .expect("disk identity comparison")
        );
    }

    #[test]
    fn same_kind_pipe_and_character_handles_use_kernel_object_identity() {
        for kind in [HandleKind::Pipe, HandleKind::Character] {
            assert!(
                standard_handles_alias_by_kind(
                    kind,
                    kind,
                    || panic!("non-disk handles must not use file identity"),
                    || Ok(true),
                )
                .expect("kernel-object comparison")
            );
        }
    }

    #[test]
    fn unknown_standard_handle_kind_fails_closed() {
        for kinds in [
            (HandleKind::Unknown, HandleKind::Disk),
            (HandleKind::Disk, HandleKind::Unknown),
        ] {
            assert!(matches!(
                standard_handles_alias_by_kind(kinds.0, kinds.1, || Ok(false), || Ok(false)),
                Err(SysError::Io(_))
            ));
        }
    }

    #[test]
    fn cas_create_preflight_treats_non_regular_observation_as_stale() {
        assert!(matches!(
            cas_create_preflight(Err(SysError::NotRegularFile)),
            Ok(Some(CasOutcome::Stale))
        ));
        assert!(matches!(
            cas_create_preflight(Ok(true)),
            Ok(Some(CasOutcome::Stale))
        ));
        assert!(matches!(cas_create_preflight(Ok(false)), Ok(None)));
    }
}
