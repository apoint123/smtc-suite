use ferrous_opencc::{OpenCC, error::OpenCCError};
use log::{error, info, trace};
use once_cell::sync::Lazy;

// 全局静态变量，用于存储一次性初始化后的 OpenCC 转换器实例或初始化错误。
// Lazy 块仅在首次访问时执行。
static OPENCC_T2S_CONVERTER: Lazy<Result<OpenCC, OpenCCError>> = Lazy::new(|| {
    trace!("[简繁转换] 首次尝试初始化 ferrous-opencc T2S 转换器 (Lazy 模式)...");
    OpenCC::from_config_name("t2s.json")
});

/// 获取对已初始化的 T2S (繁体转简体) OpenCC 转换器的静态引用。
fn get_t2s_converter() -> Result<&'static OpenCC, &'static OpenCCError> {
    OPENCC_T2S_CONVERTER.as_ref()
}

/// 将文本从繁体中文转换为简体中文。
///
/// 如果转换器初始化失败，或者在转换过程中发生错误，会记录一条错误日志并返回原始文本，
/// 确保程序的健壮性。
///
/// # 参数
/// * `text`: 需要进行转换的字符串切片。
///
/// # 返回
/// - 返回转换后的简体中文字符串。
/// - 如果输入为空字符串，直接返回空字符串。
/// - 如果转换失败，返回原始输入字符串。
pub fn convert_traditional_to_simplified(text: &str) -> String {
    if text.is_empty() {
        return String::new();
    }

    match get_t2s_converter() {
        Ok(converter) => {
            let simplified_text = converter.convert(text);

            if text != simplified_text {
                info!("[简繁转换] 原文: '{text}' -> 简体: '{simplified_text}'");
            } else if !text.is_empty() && text.chars().any(|c| c as u32 > 127 && c != ' ') {
                trace!("[简繁转换] 文本 '{text}' 转换为简体后无变化 (可能已是简体或无对应转换)。");
            }
            simplified_text
        }
        Err(e) => {
            // 初始化或获取转换器失败
            error!("[简繁转换] 获取 OpenCC T2S 转换器失败: {e}。繁简转换功能将不可用。返回原文。");
            text.to_string()
        }
    }
}

/// 从 SMTC 会话 ID（通常是 AUMID 或可执行文件名）中提取一个更易读的显示名称。
///
/// ## 解析逻辑
/// 1.  **对于标准可执行文件名** (如 "Spotify.exe"):
///     直接返回文件名本身，因为这已经足够清晰。
/// 2.  **对于 UWP 应用的 AUMID** (如 "`AppleInc.AppleMusicWin_nzyj5cx40ttqa!App`"):
///     a. 取 `!` 之前的部分: "`AppleInc.AppleMusicWin_nzyj5cx40ttqa`"
///     b. 取 `_` 之前的部分: "`AppleInc.AppleMusicWin`"
///     c. 取最后一个 `.` 之后的部分: "`AppleMusicWin`"
///     d. 对其进行处理：
///        - 在小写字母和大写字母之间插入空格: "`Apple Music Win`"
///        - 移除常见的后缀（如 "Win", "UWP"）并修剪空格: "`Apple Music`"
/// 3.  如果任何步骤失败，则返回原始 ID 作为后备。
pub fn get_display_name_from_smtc_id(id_str: &str) -> String {
    // 1: 如果不包含 '!'，很可能是一个简单的可执行文件名。
    if !id_str.contains('!') {
        return id_str.to_string();
    }

    // 2: 处理 UWP 的 AUMID 格式。
    // 格式: Publisher.AppName_PublisherId!ApplicationId
    let prettified_name = id_str
        .split('!')
        .next() // 取 '!' 之前的部分
        .and_then(|pfn| pfn.split('_').next()) // 取 '_' 之前的部分
        .and_then(|name_part| name_part.rsplit('.').next()) // 取最后一个 '.' 之后的部分
        .map(|app_name| {
            // 处理文本
            let mut pretty = String::with_capacity(app_name.len() + 5);
            let mut chars = app_name.chars().peekable();
            while let Some(current) = chars.next() {
                pretty.push(current);
                if let Some(&next) = chars.peek() {
                    // 在小写和大写字母之间插入空格
                    if current.is_lowercase() && next.is_uppercase() {
                        pretty.push(' ');
                    }
                }
            }
            // 移除常见后缀并清理
            pretty
                .trim_end_matches("Win")
                .trim_end_matches("Uwp")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty()); // 确保结果不是空字符串

    // 如果所有处理都成功，则返回处理后的名称，否则返回原始 ID。
    prettified_name.unwrap_or_else(|| id_str.to_string())
}
