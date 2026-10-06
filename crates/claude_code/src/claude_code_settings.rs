use settings::{ClaudeCodeSettingsContent, RegisterSetting, Settings, SettingsContent};

#[derive(Debug, Clone, Copy, RegisterSetting)]
pub struct ClaudeCodeSettings {
    pub enabled: bool,
}

impl Settings for ClaudeCodeSettings {
    fn from_settings(content: &SettingsContent) -> Self {
        let claude_code: &ClaudeCodeSettingsContent = content.claude_code.as_ref().unwrap();

        Self {
            enabled: claude_code.enabled.unwrap(),
        }
    }
}
