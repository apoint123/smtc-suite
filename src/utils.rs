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
                info!("[简繁转换] 原文: '{}' -> 简体: '{}'", text, simplified_text);
            } else if !text.is_empty() && text.chars().any(|c| c as u32 > 127 && c != ' ') {
                trace!(
                    "[简繁转换] 文本 '{}' 转换为简体后无变化 (可能已是简体或无对应转换)。",
                    text
                );
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
