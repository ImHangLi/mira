//! Fixed contract limits. Byte limits count UTF-8 bytes, not characters.

pub const KIB: usize = 1024;
pub const MIB: usize = 1024 * KIB;

/// Largest integer accepted anywhere on the wire (2^53 - 1).
pub const MAX_SAFE_INTEGER: u64 = (1 << 53) - 1;

pub const MAX_JSON_DEPTH: usize = 64;
pub const MAX_LPP_FRAME_BYTES: usize = MIB;
pub const MAX_IPC_FRAME_BYTES: usize = 2 * MIB;
pub const MAX_MANIFEST_BYTES: usize = MIB;
pub const MAX_PUBLIC_REPLY_BYTES: usize = MIB;
pub const DEFAULT_REPLY_BUDGET_BYTES: usize = 32 * KIB;
pub const MAX_REPLY_BUDGET_BYTES: usize = 256 * KIB;

pub const MAX_NAME_BYTES: usize = 128;
pub const MAX_DESCRIPTION_BYTES: usize = 1024;
pub const MAX_TAGS: usize = 12;
pub const MAX_TAG_BYTES: usize = 48;
pub const MAX_PLUGINS: usize = 256;
pub const MAX_ACTIONS: usize = 128;
pub const MAX_VIEWS: usize = 64;
pub const MAX_ENV_FILES: usize = 16;
pub const MAX_ARGV_BYTES: usize = 64 * KIB;
pub const MAX_META_BYTES: usize = 64 * KIB;
pub const MAX_CLIENT_ENV_BYTES: usize = 256 * KIB;

pub const MAX_SCHEMA_BYTES: usize = 256 * KIB;
pub const MAX_SCHEMA_DEPTH: usize = 32;

pub const MAX_LOG_TEXT_BYTES: usize = 8 * KIB;
pub const MAX_MESSAGE_BYTES: usize = 2 * KIB;
pub const MAX_ERROR_MESSAGE_BYTES: usize = 4 * KIB;
pub const MAX_ERROR_DETAILS_BYTES: usize = 8 * KIB;

pub const MAX_TEXT_VIEW_BYTES: usize = MIB;
pub const MAX_TABLE_COLUMNS: usize = 64;
pub const MAX_TABLE_ROWS: usize = 10_000;
pub const MAX_ROW_ID_BYTES: usize = 128;
pub const MAX_TREE_DEPTH: usize = 32;
pub const MAX_TREE_NODES: usize = 10_000;

pub const MAX_REQUEST_KEY_BYTES: usize = 128;
pub const MAX_CURSOR_BYTES: usize = 2048;

pub const TIMEOUT_AFTER_MAX_MS: u64 = 604_800_000;
pub const DEFAULT_TASK_TIMEOUT_MS: u64 = 300_000;
pub const DEFAULT_STOP_GRACE_MS: u64 = 5_000;
pub const STOP_GRACE_RANGE_MS: (u64, u64) = (100, 60_000);
pub const CLEANUP_TIMEOUT_MS: u64 = 10_000;
pub const MIN_SCHEDULE_EVERY_MS: u64 = 1_000;
pub const DEFAULT_BACKGROUND_TTL_MS: u64 = 2 * 60 * 60 * 1000;

/// PTY screen size before any writer resizes it.
pub const DEFAULT_TERMINAL_COLS: u16 = 100;
pub const DEFAULT_TERMINAL_ROWS: u16 = 30;
pub const MAX_TERMINAL_COLS: u16 = 400;
pub const MAX_TERMINAL_ROWS: u16 = 200;
/// Largest single `terminal.input` body; larger input is refused, never truncated.
pub const MAX_TERMINAL_INPUT_BYTES: usize = 64 * KIB;
