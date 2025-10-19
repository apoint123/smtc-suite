use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

use ferrous_opencc::OpenCC;

pub fn convert_text(text: &str, converter: Option<&OpenCC>) -> String {
    if text.is_empty() {
        return String::new();
    }

    converter.map_or_else(|| text.to_string(), |converter| converter.convert(text))
}

pub fn get_display_name_from_smtc_id(id_str: &str) -> String {
    if !id_str.contains('!') {
        return id_str.to_string();
    }

    let prettified_name = id_str
        .split('!')
        .next()
        .and_then(|pfn| pfn.split('_').next())
        .and_then(|name_part| name_part.rsplit('.').next())
        .map(|app_name| {
            let mut pretty = String::with_capacity(app_name.len() + 5);
            let mut chars = app_name.chars().peekable();
            while let Some(current) = chars.next() {
                pretty.push(current);
                if let Some(&next) = chars.peek()
                    && current.is_lowercase()
                    && next.is_uppercase()
                {
                    pretty.push(' ');
                }
            }
            pretty
                .trim_end_matches("Win")
                .trim_end_matches("Uwp")
                .trim()
                .to_string()
        })
        .filter(|s| !s.is_empty());

    prettified_name.unwrap_or_else(|| id_str.to_string())
}

pub struct StaFuture<F: Future> {
    future: Pin<Box<F>>,
}

pub fn await_in_sta<F: Future>(future: F) -> StaFuture<F> {
    StaFuture {
        future: Box::pin(future),
    }
}

impl<F: Future> Future for StaFuture<F> {
    type Output = F::Output;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Poll::Ready(output) = self.future.as_mut().poll(cx) {
            return Poll::Ready(output);
        }

        crate::smtc_handler::pump_pending_messages();

        cx.waker().wake_by_ref();

        Poll::Pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrous_opencc::{OpenCC, config::BuiltinConfig};

    #[test]
    fn test_convert_text_with_converter() {
        let converter = OpenCC::from_config(BuiltinConfig::S2t).unwrap();
        let input = "一个项目";
        let expected = "一個項目";
        assert_eq!(convert_text(input, Some(&converter)), expected);
    }

    #[test]
    fn test_convert_text_without_converter() {
        let input = "Hello, World";
        assert_eq!(convert_text(input, None), input.to_string());
    }

    #[test]
    fn test_convert_text_with_empty_string() {
        let converter = OpenCC::from_config(BuiltinConfig::S2t).unwrap();
        assert_eq!(convert_text("", Some(&converter)), "");
        assert_eq!(convert_text("", None), "");
    }

    #[test]
    fn test_display_name_from_plain_exe() {
        let id = "Spotify.exe";
        assert_eq!(get_display_name_from_smtc_id(id), "Spotify.exe");
    }

    #[test]
    fn test_display_name_from_apple_music_aumid() {
        let id = "AppleInc.AppleMusicWin_nzyj5cx40ttqa!App";
        assert_eq!(get_display_name_from_smtc_id(id), "Apple Music");
    }

    #[test]
    fn test_display_name_from_zune_aumid() {
        let id = "Microsoft.ZuneMusic_8wekyb3d8bbwe!Microsoft.ZuneMusic";
        assert_eq!(get_display_name_from_smtc_id(id), "Zune Music");
    }

    #[test]
    fn test_display_name_from_another_camelcase_aumid() {
        let id = "SomeDeveloper.MyAwesomePlayerUwp_abcdef123!Main";
        assert_eq!(get_display_name_from_smtc_id(id), "My Awesome Player");
    }

    #[test]
    fn test_display_name_fallback_when_prettify_is_empty() {
        let id = "Vendor.Win_hash!App";
        assert_eq!(get_display_name_from_smtc_id(id), id);
    }

    #[test]
    fn test_display_name_with_empty_input() {
        assert_eq!(get_display_name_from_smtc_id(""), "");
    }

    #[test]
    fn test_display_name_with_no_dots_in_pfn() {
        let id = "MyCoolApp_hash!App";
        assert_eq!(get_display_name_from_smtc_id(id), "My Cool App");
    }
}
