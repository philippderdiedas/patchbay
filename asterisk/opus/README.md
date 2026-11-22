# Opus transcoder (open source)

`codec_opus_open_source.c` and `ex_opus.h` from https://github.com/traud/asterisk-opus
(commit a959f072d3f364be983dd27e6e250b038aaef747), GPL-2.0 like Asterisk itself (author Lorenzo Miniero, Meetecho); links against
libopus (BSD). Built into the image instead of Sangoma's binary `codec_opus`, whose licence
does not cover redistributing it.

Only the transcoder is taken: Asterisk 24 already has the Opus format, `res_format_attr_opus`
and `asterisk/opus.h`.
