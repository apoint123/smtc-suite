use ferrous_opencc::OpenCC;

/// 使用指定的 `OpenCC` 实例转换文本。
///
/// # 参数
/// * `text`: 需要转换的字符串切片。
/// * `converter`: 一个 `Option<&OpenCC>`，如果为 `Some`，则用其转换；如果为 `None`，则不进行任何转换。
///
/// # 返回
/// - 如果提供了转换器，则返回转换后的字符串。
/// - 如果转换器为 `None`，则返回原始输入字符串。
pub fn convert_text(text: &str, converter: Option<&OpenCC>) -> String {
    if text.is_empty() {
        return String::new();
    }

    converter.map_or_else(|| text.to_string(), |converter| converter.convert(text))
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
