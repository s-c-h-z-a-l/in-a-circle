use subtitles::{INPUT_BYTES, Loss, OUTPUT_BYTES, ReadError};

#[test]
fn loss_order_is_the_warning_order() {
    let mut all = vec![
        Loss::Brackets,
        Loss::VoiceNames,
        Loss::Settings,
        Loss::Drawing,
        Loss::Karaoke,
        Loss::Position,
        Loss::Styling,
    ];
    all.sort();
    assert_eq!(
        all,
        [
            Loss::Styling,
            Loss::Position,
            Loss::Karaoke,
            Loss::Drawing,
            Loss::Settings,
            Loss::VoiceNames,
            Loss::Brackets,
        ]
    );
}

#[test]
fn loss_messages_are_the_user_text() {
    assert_eq!(Loss::Styling.message(), "colours, fonts and sizes removed");
    assert_eq!(Loss::Position.message(), "exact positions removed");
    assert_eq!(Loss::Karaoke.message(), "karaoke effects removed");
    assert_eq!(Loss::Drawing.message(), "drawings removed");
    assert_eq!(Loss::Settings.message(), "style and region blocks ignored");
    assert_eq!(
        Loss::VoiceNames.message(),
        "speaker names written as 'Name:' prefixes"
    );
    assert_eq!(
        Loss::Brackets.message(),
        "characters that look like markup written as look-alikes"
    );
}

#[test]
fn read_errors_say_what_the_user_sees() {
    assert_eq!(
        ReadError::NotUtf8 { at: 7 }.to_string(),
        "not UTF-8 text (byte at 7): convert it with Encoding first"
    );
    assert_eq!(
        ReadError::TooLarge.to_string(),
        "subtitle file is larger than 8 MiB"
    );
    assert_eq!(ReadError::NoCues.to_string(), "no subtitles found");
    assert_eq!(
        ReadError::Malformed {
            line: 7,
            reason: "mismatched end tag"
        }
        .to_string(),
        "not well-formed TTML (line 7): mismatched end tag"
    );
    assert_eq!(ReadError::NotTtml.to_string(), "not a TTML document");
    assert_eq!(
        ReadError::Unsupported("clock time base").to_string(),
        "unsupported TTML: clock time base"
    );
}

#[test]
fn limits_are_the_documented_sizes() {
    // The message above names 8 MiB: the constant and the text must agree.
    assert_eq!(INPUT_BYTES, 8 * 1024 * 1024);
    assert_eq!(OUTPUT_BYTES, 32 * 1024 * 1024);
}
