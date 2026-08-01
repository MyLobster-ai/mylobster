//! Telegram native command menu management and group-command admin gating.
//!
//! Ports of OpenClaw `extensions/telegram/src/bot-native-command-menu.ts`
//! (register/clear menus in the default and all_group_chats scopes, command
//! budget fitting) at v2026.7.1, plus the super-group admin-only command
//! gating carried over from v2026.4.9.

pub const TELEGRAM_MAX_COMMANDS: usize = 100;
pub const TELEGRAM_TOTAL_COMMAND_TEXT_BUDGET: usize = 5700;
pub const TELEGRAM_MIN_COMMAND_DESCRIPTION_LENGTH: usize = 1;
pub const TELEGRAM_MAX_COMMAND_DESCRIPTION_LENGTH: usize = 256;

/// A command shown in the Telegram command menu.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
pub struct TelegramMenuCommand {
    pub command: String,
    pub description: String,
    /// Per-language description overrides keyed by ISO-639-1 code
    /// (native command localization, v2026.7.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description_localizations: Option<std::collections::HashMap<String, String>>,
}

/// Command menu scopes managed by the channel: the default scope and the
/// group-chat scope (upstream `TELEGRAM_COMMAND_MENU_SCOPES`). Registering /
/// clearing in both keeps DM and group command menus consistent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TelegramCommandMenuScope {
    Default,
    AllGroupChats,
}

pub const TELEGRAM_COMMAND_MENU_SCOPES: [TelegramCommandMenuScope; 2] = [
    TelegramCommandMenuScope::Default,
    TelegramCommandMenuScope::AllGroupChats,
];

impl TelegramCommandMenuScope {
    /// The `scope` object for setMyCommands/deleteMyCommands, `None` for the
    /// default scope.
    pub fn api_scope(&self) -> Option<serde_json::Value> {
        match self {
            Self::Default => None,
            Self::AllGroupChats => Some(serde_json::json!({ "type": "all_group_chats" })),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::AllGroupChats => "all_group_chats",
        }
    }
}

/// Operation label for logs (upstream `formatTelegramCommandScopeOperation`):
/// `setMyCommands` for the default scope, `setMyCommands(all_group_chats)`
/// otherwise.
pub fn format_command_scope_operation(operation: &str, scope: TelegramCommandMenuScope) -> String {
    match scope {
        TelegramCommandMenuScope::Default => operation.to_string(),
        _ => format!("{operation}({})", scope.label()),
    }
}

fn count_command_text(value: &str) -> usize {
    // Upstream counts Unicode code points, not UTF-16 units.
    value.chars().count()
}

fn truncate_command_text(value: &str, max_length: usize) -> String {
    if max_length == 0 {
        return String::new();
    }
    if count_command_text(value) <= max_length {
        return value.to_string();
    }
    let suffix = if max_length > 1 { "…" } else { "" };
    let prefix_limit = max_length - count_command_text(suffix);
    let prefix: String = value.chars().take(prefix_limit).collect();
    format!("{prefix}{suffix}")
}

/// Result of fitting commands within the Bot API total text budget.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FittedCommands {
    pub commands: Vec<TelegramMenuCommand>,
    pub description_trimmed: bool,
    pub text_budget_drop_count: usize,
}

/// Fits commands within `max_total_chars` (upstream
/// `fitTelegramCommandsWithinTextBudget`): drops trailing commands until the
/// remaining descriptions fit at ≥ 1 char each, then evenly caps description
/// length (≤ 256).
pub fn fit_commands_within_text_budget(
    commands: &[TelegramMenuCommand],
    max_total_chars: usize,
) -> FittedCommands {
    let mut candidate: Vec<TelegramMenuCommand> = commands.to_vec();
    while !candidate.is_empty() {
        let command_name_chars: usize = candidate.iter().map(|c| count_command_text(&c.command)).sum();
        let description_budget = max_total_chars.saturating_sub(command_name_chars);
        let minimum_description_budget = candidate.len() * TELEGRAM_MIN_COMMAND_DESCRIPTION_LENGTH;
        if description_budget < minimum_description_budget
            || max_total_chars < command_name_chars
        {
            candidate.pop();
            continue;
        }
        let description_cap = (description_budget / candidate.len())
            .max(TELEGRAM_MIN_COMMAND_DESCRIPTION_LENGTH)
            .min(TELEGRAM_MAX_COMMAND_DESCRIPTION_LENGTH);
        let mut description_trimmed = false;
        let fitted: Vec<TelegramMenuCommand> = candidate
            .iter()
            .map(|c| {
                let description = truncate_command_text(&c.description, description_cap);
                if description != c.description {
                    description_trimmed = true;
                }
                TelegramMenuCommand {
                    command: c.command.clone(),
                    description,
                    description_localizations: c.description_localizations.clone(),
                }
            })
            .collect();
        return FittedCommands {
            text_budget_drop_count: commands.len() - fitted.len(),
            commands: fitted,
            description_trimmed,
        };
    }
    FittedCommands {
        commands: Vec::new(),
        description_trimmed: false,
        text_budget_drop_count: commands.len(),
    }
}

/// Caps the menu at `TELEGRAM_MAX_COMMANDS` and fits the text budget.
pub fn build_capped_menu_commands(commands: &[TelegramMenuCommand]) -> FittedCommands {
    let capped: Vec<TelegramMenuCommand> =
        commands.iter().take(TELEGRAM_MAX_COMMANDS).cloned().collect();
    let mut fitted = fit_commands_within_text_budget(&capped, TELEGRAM_TOTAL_COMMAND_TEXT_BUDGET);
    fitted.text_budget_drop_count += commands.len().saturating_sub(capped.len());
    fitted
}

// ============================================================================
// Native command localization (bot-native-command-menu.ts, v2026.7.1)
// ============================================================================

/// Normalizes a language code to lowercase ISO-639-1, or `None` when the Bot
/// API cannot accept it (upstream `normalizeTelegramLanguageCode`).
pub fn normalize_telegram_language_code(language_code: &str) -> Option<String> {
    let normalized = language_code.trim().to_lowercase();
    (normalized.len() == 2 && normalized.bytes().all(|b| b.is_ascii_lowercase()))
        .then_some(normalized)
}

fn read_localized_description(
    command: &TelegramMenuCommand,
    language_code: &str,
) -> Option<String> {
    let localizations = command.description_localizations.as_ref()?;
    for (raw_language, raw_description) in localizations {
        if normalize_telegram_language_code(raw_language).as_deref() != Some(language_code) {
            continue;
        }
        let description = raw_description.trim();
        if !description.is_empty() {
            return Some(description.to_string());
        }
    }
    None
}

/// A per-language command menu variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalizedCommandVariant {
    pub language_code: String,
    pub commands: Vec<TelegramMenuCommand>,
}

/// Localized menu variants + language codes the Bot API cannot register
/// (upstream `buildLocalizedCommandVariants`): one variant per locale seen in
/// any command, falling back to the default description, each fit within the
/// menu text budget; locale order is sorted for stable registration.
pub fn build_localized_command_variants(
    commands: &[TelegramMenuCommand],
) -> (Vec<LocalizedCommandVariant>, Vec<String>) {
    let mut locales = std::collections::BTreeSet::new();
    let mut unsupported = std::collections::BTreeSet::new();
    for command in commands {
        if let Some(localizations) = &command.description_localizations {
            for language in localizations.keys() {
                match normalize_telegram_language_code(language) {
                    Some(normalized) => {
                        locales.insert(normalized);
                    }
                    None => {
                        unsupported.insert(language.clone());
                    }
                }
            }
        }
    }
    let variants = locales
        .into_iter()
        .map(|language_code| {
            let localized: Vec<TelegramMenuCommand> = commands
                .iter()
                .map(|command| TelegramMenuCommand {
                    command: command.command.clone(),
                    description: read_localized_description(command, &language_code)
                        .unwrap_or_else(|| command.description.clone()),
                    description_localizations: None,
                })
                .collect();
            LocalizedCommandVariant {
                commands: fit_commands_within_text_budget(
                    &localized,
                    TELEGRAM_TOTAL_COMMAND_TEXT_BUDGET,
                )
                .commands,
                language_code,
            }
        })
        .collect();
    (variants, unsupported.into_iter().collect())
}

// ============================================================================
// Super-group support: admin-only command gating (carryover v2026.4.9)
// ============================================================================

/// Whether a Telegram chat type is a group chat. Super groups are treated
/// exactly like groups (upstream: `chat.type === "group" || "supergroup"`).
pub fn is_group_chat_type(chat_type: &str) -> bool {
    matches!(chat_type, "group" | "supergroup")
}

/// Whether a `getChatMember` status grants admin rights.
pub fn is_chat_admin_status(status: &str) -> bool {
    matches!(status, "creator" | "administrator")
}

/// Gate for control commands issued in group/super-group chats: commands are
/// admin-only by default (super-group support, v2026.4.9), overridable via
/// `adminOnlyCommands` (per-group config wins over the account default).
/// Explicitly allowlisted senders always pass.
pub fn should_allow_group_command(params: GroupCommandGate) -> bool {
    if !is_group_chat_type(params.chat_type) {
        return true;
    }
    if params.sender_allowlisted {
        return true;
    }
    let admin_only = params
        .group_admin_only_commands
        .or(params.account_admin_only_commands)
        .unwrap_or(true);
    if !admin_only {
        return true;
    }
    params
        .sender_status
        .map(is_chat_admin_status)
        .unwrap_or(false)
}

/// Inputs for [`should_allow_group_command`].
#[derive(Debug, Clone, Copy, Default)]
pub struct GroupCommandGate<'a> {
    /// Telegram chat type: `private`, `group`, `supergroup`, `channel`.
    pub chat_type: &'a str,
    /// `getChatMember` status of the sender, when known.
    pub sender_status: Option<&'a str>,
    /// Sender explicitly present in `allowFrom` / `groupAllowFrom`.
    pub sender_allowlisted: bool,
    /// Per-group `adminOnlyCommands` override.
    pub group_admin_only_commands: Option<bool>,
    /// Account-level `adminOnlyCommands` default.
    pub account_admin_only_commands: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cmd(name: &str, desc: &str) -> TelegramMenuCommand {
        TelegramMenuCommand {
            command: name.to_string(),
            description: desc.to_string(),
            description_localizations: None,
        }
    }

    fn localized_cmd(
        name: &str,
        desc: &str,
        localizations: &[(&str, &str)],
    ) -> TelegramMenuCommand {
        TelegramMenuCommand {
            command: name.to_string(),
            description: desc.to_string(),
            description_localizations: Some(
                localizations
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ),
        }
    }

    // ---- localization ----

    #[test]
    fn language_code_normalization() {
        assert_eq!(normalize_telegram_language_code(" DE "), Some("de".to_string()));
        assert_eq!(normalize_telegram_language_code("pt-BR"), None);
        assert_eq!(normalize_telegram_language_code("deu"), None);
        assert_eq!(normalize_telegram_language_code(""), None);
    }

    #[test]
    fn localized_variants_built_per_locale() {
        let commands = vec![
            localized_cmd("help", "Show help", &[("de", "Hilfe anzeigen"), ("FR", "Aide")]),
            cmd("status", "Show status"),
        ];
        let (variants, unsupported) = build_localized_command_variants(&commands);
        assert!(unsupported.is_empty());
        assert_eq!(variants.len(), 2);
        // Sorted locale order.
        assert_eq!(variants[0].language_code, "de");
        assert_eq!(variants[1].language_code, "fr");
        // Localized description used; unlocalized falls back to default.
        assert_eq!(variants[0].commands[0].description, "Hilfe anzeigen");
        assert_eq!(variants[0].commands[1].description, "Show status");
        assert_eq!(variants[1].commands[0].description, "Aide");
    }

    #[test]
    fn unsupported_language_codes_reported() {
        let commands = vec![localized_cmd("go", "Go", &[("pt-BR", "Ir"), ("es", "Vamos")])];
        let (variants, unsupported) = build_localized_command_variants(&commands);
        assert_eq!(variants.len(), 1);
        assert_eq!(variants[0].language_code, "es");
        assert_eq!(unsupported, vec!["pt-BR".to_string()]);
    }

    #[test]
    fn no_localizations_no_variants() {
        let (variants, unsupported) = build_localized_command_variants(&[cmd("a", "b")]);
        assert!(variants.is_empty());
        assert!(unsupported.is_empty());
    }

    // ---- scopes ----

    #[test]
    fn menu_scopes_cover_default_and_group_chats() {
        assert_eq!(TELEGRAM_COMMAND_MENU_SCOPES.len(), 2);
        assert!(TELEGRAM_COMMAND_MENU_SCOPES[0].api_scope().is_none());
        assert_eq!(
            TELEGRAM_COMMAND_MENU_SCOPES[1].api_scope().unwrap()["type"],
            "all_group_chats"
        );
    }

    #[test]
    fn scope_operation_labels() {
        assert_eq!(
            format_command_scope_operation("setMyCommands", TelegramCommandMenuScope::Default),
            "setMyCommands"
        );
        assert_eq!(
            format_command_scope_operation(
                "deleteMyCommands",
                TelegramCommandMenuScope::AllGroupChats
            ),
            "deleteMyCommands(all_group_chats)"
        );
    }

    // ---- budget fitting ----

    #[test]
    fn commands_within_budget_unchanged() {
        let commands = vec![cmd("help", "Show help"), cmd("status", "Show status")];
        let fitted = fit_commands_within_text_budget(&commands, 5700);
        assert_eq!(fitted.commands, commands);
        assert!(!fitted.description_trimmed);
        assert_eq!(fitted.text_budget_drop_count, 0);
    }

    #[test]
    fn long_descriptions_trimmed_to_cap() {
        let commands = vec![cmd("a", &"d".repeat(500))];
        let fitted = fit_commands_within_text_budget(&commands, 5700);
        assert!(fitted.description_trimmed);
        let desc = &fitted.commands[0].description;
        assert!(desc.chars().count() <= TELEGRAM_MAX_COMMAND_DESCRIPTION_LENGTH);
        assert!(desc.ends_with('…'));
    }

    #[test]
    fn over_budget_drops_trailing_commands() {
        // 100 commands with 60-char names = 6000 name chars > 5700 budget.
        let commands: Vec<_> = (0..100)
            .map(|i| cmd(&format!("{:0>60}", i), "d"))
            .collect();
        let fitted = fit_commands_within_text_budget(&commands, 5700);
        assert!(fitted.commands.len() < 100);
        assert!(fitted.text_budget_drop_count > 0);
    }

    #[test]
    fn menu_capped_at_100_commands() {
        let commands: Vec<_> = (0..150).map(|i| cmd(&format!("c{i}"), "d")).collect();
        let fitted = build_capped_menu_commands(&commands);
        assert!(fitted.commands.len() <= TELEGRAM_MAX_COMMANDS);
        assert!(fitted.text_budget_drop_count >= 50);
    }

    #[test]
    fn truncate_command_text_counts_code_points() {
        assert_eq!(truncate_command_text("héllo wörld", 5), "héll…");
        assert_eq!(truncate_command_text("short", 10), "short");
        assert_eq!(truncate_command_text("ab", 1), "a");
    }

    // ---- admin gating ----

    #[test]
    fn private_chats_never_gated() {
        assert!(should_allow_group_command(GroupCommandGate {
            chat_type: "private",
            ..Default::default()
        }));
    }

    #[test]
    fn supergroup_commands_admin_only_by_default() {
        assert!(!should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("member"),
            ..Default::default()
        }));
        assert!(should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("administrator"),
            ..Default::default()
        }));
        assert!(should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("creator"),
            ..Default::default()
        }));
    }

    #[test]
    fn regular_groups_gated_like_supergroups() {
        assert!(!should_allow_group_command(GroupCommandGate {
            chat_type: "group",
            sender_status: Some("member"),
            ..Default::default()
        }));
    }

    #[test]
    fn allowlisted_sender_bypasses_admin_gate() {
        assert!(should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("member"),
            sender_allowlisted: true,
            ..Default::default()
        }));
    }

    #[test]
    fn group_override_beats_account_default() {
        // Account says admin-only, group disables it.
        assert!(should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("member"),
            group_admin_only_commands: Some(false),
            account_admin_only_commands: Some(true),
            ..Default::default()
        }));
        // Account disables, group re-enables.
        assert!(!should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: Some("member"),
            group_admin_only_commands: Some(true),
            account_admin_only_commands: Some(false),
            ..Default::default()
        }));
    }

    #[test]
    fn unknown_sender_status_denied_when_admin_only() {
        assert!(!should_allow_group_command(GroupCommandGate {
            chat_type: "supergroup",
            sender_status: None,
            ..Default::default()
        }));
    }
}
