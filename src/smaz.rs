// Originally based on Smaz by Salvatore Sanfilippo.
// Copyright (c) 2006-2009, Salvatore Sanfilippo.
//
// Smaz is licensed under the 3-clause BSD license.
// See LICENSES/SMAZ.txt.
//
// This file contains a Rust port and modifications of the original
// Smaz compression implementation.

// const SMAZ_CB: [&[u8]; 242]
const SMAZ_CB: [&[u8]; 241] = [
    b"\x02s,\xb6",
    b"\x03had\x9a\x02leW",
    b"\x03on \x8e",
    b"",
    b"\x01yS",
    b"\x02ma\xad\x02li\x97",
    b"\x03or \xb0",
    b"",
    b"\x02ll\x98\x03s t\xbf",
    b"\x04fromg\x02mel",
    b"",
    b"\x03its\xda",
    b"\x01z\xdb",
    b"\x03ingF",
    b"\x01>\xde",
    b"\x01 \x00\x03   (\x02nc\xe4",
    b"\x02nd=\x03 on\xca",
    b"\x02ne\x8b\x03hat\xbe\x03re q",
    b"",
    b"\x02ngT\x03herz\x04have\xc6\x03s o\x95",
    b"",
    b"\x03ionk\x03s a\xac\x02ly\xea",
    b"\x03hisL\x03 inN\x03 be\xaa",
    b"",
    b"\x03 fo\xd5\x03 of \x03 ha\xc9",
    b"",
    b"\x02of\x05",
    b"\x03 co\xa1\x02no\xb7\x03 ma\xf8",
    b"",
    b"",
    b"\x03 cl\xee\x03enta\x03 an7",
    b"\x02ns\xc0\x01\"e",
    b"\x03n t\x8f\x02ntP\x03s, \x85",
    b"\x02pe\xd0\x03 we\xe9\x02om\x93",
    b"\x02on\x1f",
    b"",
    b"\x02y G",
    b"\x03 wa\xb9",
    b"\x03 re\xd1\x02or*",
    b"",
    b"\x02=\"\xa9\x02ot\xdf",
    b"\x03forD\x02ou[",
    b"\x03 toR",
    b"\x03 th\r",
    b"\x03 it\xf6",
    b"\x03but\xb1\x02ra\x82\x03 wi\xf3\x02</\xf1",
    b"\x03 wh\x9f",
    b"\x02  4",
    b"\x03nd ?",
    b"\x02re!",
    b"",
    b"\x03ng c",
    b"",
    b"\x03ly \xc7\x03ass\xd3\x01a\x04\x02rir",
    b"",
    b"",
    b"",
    b"\x02se_",
    b"\x03of \"",
    b"\x03div\xf4\x02ros\x03ere\xa0",
    b"",
    b"\x02ta\xc8\x01bZ\x02si\xd4",
    b"",
    b"\x03and\x07\x02rs\xdd",
    b"\x02rt\xf2",
    b"\x02teE",
    b"\x03ati\xce",
    b"\x02so\xb3",
    b"\x02th\x11",
    b"\x02tiJ\x01c\x1c\x03allp",
    b"\x03ate\xe5",
    b"\x02ss\xa6",
    b"\x02stM",
    b"",
    b"\x02><\xe6",
    b"\x02to\x14",
    b"\x03arew",
    b"\x01d\x18",
    b"\x02tr\xc3",
    b"",
    b"\x01\n1\x03 a \x92",
    b"\x03f tv\x02veo",
    b"\x02un\xe0",
    b"",
    b"\x03e o\xa2",
    b"\x02a \xa3\x02wa\xd6\x01e\x02",
    b"\x02ur\x96\x03e a\xbc",
    b"\x02us\xa4\x03\n\r\n\xa7",
    b"\x02ut\xc4\x03e c\xfb",
    b"\x02we\x91",
    b"",
    b"",
    b"\x02wh\xc2",
    b"\x01f,",
    b"",
    b"",
    b"",
    b"\x03d t\x86",
    b"",
    b"",
    b"\x03th \xe3",
    b"\x01g;",
    b"",
    b"",
    b"\x01\r9\x03e s\xb5",
    b"\x03e t\x9c",
    b"",
    b"\x03to Y",
    b"\x03e\r\n\x9e",
    b"\x02d \x1e\x01h\x12",
    b"",
    b"\x01,Q",
    b"\x02 a\x19",
    b"\x02 b^",
    b"\x02\r\n\x15\x02 cI",
    b"\x02 d\xa5",
    b"\x02 e\xab",
    b"\x02 fh\x01i\x08\x02e \x0b",
    b"",
    b"\x02 hU\x01-\xcc",
    b"\x02 i8",
    b"",
    b"",
    b"\x02 l\xcd",
    b"\x02 m{",
    b"\x02f :\x02 n\xec",
    b"\x02 o\x1d",
    b"\x02 p}\x01.n\x03\r\n\r\xa8",
    b"",
    b"\x02 r\xbd",
    b"\x02 s>",
    b"\x02 t\x0e",
    b"",
    b"\x02g \x9d\x05which+\x03whi\xf7",
    b"\x02 w5",
    b"\x01/\xc5",
    b"\x03as \x8c",
    b"\x03at \x87",
    b"",
    b"\x03who\xd9",
    b"",
    b"\x01l\x16\x02h \x8a",
    b"",
    b"\x02, $",
    b"",
    b"\x04withV",
    b"",
    b"",
    b"",
    b"\x01m-",
    b"",
    b"",
    b"\x02ac\xef",
    b"\x02ad\xe8",
    b"\x03TheH",
    b"",
    b"",
    b"\x04this\x9b\x01n\t",
    b"",
    b"\x02. y",
    b"",
    b"\x02alX\x03e, \xf5",
    b"\x03tio\x8d\x02be\\",
    b"\x02an\x1a\x03ver\xe7",
    b"",
    b"\x04that0\x03tha\xcb\x01o\x06",
    b"\x03was2",
    b"\x02arO",
    b"\x02as.",
    b"\x02at'\x03the\x01\x04they\x80\x05there\xd2\x05theird",
    b"\x02ce\x88",
    b"\x04were]",
    b"",
    b"\x02ch\x99\x02l \xb4\x01p<",
    b"",
    b"",
    b"\x03one\xae",
    b"",
    b"\x03he \x13\x02dej",
    b"\x03ter\xb8",
    b"\x02cou",
    b"",
    b"\x02by\x7f\x02di\x81\x02eax",
    b"",
    b"\x02ec\xd7",
    b"\x02edB",
    b"\x02ee\xeb",
    b"",
    b"",
    b"\x01r\x0c\x02n )",
    b"",
    b"",
    b"",
    b"\x02el\xb2",
    b"",
    b"\x03in i\x02en3",
    b"",
    b"\x02o `\x01s\n",
    b"",
    b"\x02er\x1b",
    b"\x03is t\x02es6",
    b"",
    b"\x02ge\xf9",
    b"\x04.com\xfd",
    b"\x02fo\xdc\x03our\xd8",
    b"\x03ch \xc1\x01t\x03",
    b"\x02hab",
    b"",
    b"\x03men\xfc",
    b"",
    b"\x02he\x10",
    b"",
    b"",
    b"\x01u&",
    b"\x02hif",
    b"",
    b"\x03not\x84\x02ic\x83",
    b"\x03ed @\x02id\xed",
    b"",
    b"",
    b"\x02ho\xbb",
    b"\x02r K\x01vm",
    b"",
    b"",
    b"",
    b"\x03t t\xaf\x02il\xf0",
    b"\x02im\xe2",
    b"\x03en \xcf\x02in\x0f",
    b"\x02io\x90",
    b"\x02s \x17\x01wA",
    b"",
    b"\x03er |",
    b"\x03es ~\x02is%",
    b"\x02it/",
    b"",
    b"\x02iv\xba",
    b"",
    b"\x02t #\x07http://C\x01x\xfa",
    b"\x02la\x89",
    b"\x01<\xe1",
    b"\x03, a\x94",
];

const SMAZ_RCB: [&str; 254] = [
    " ", "the", "e", "t", "a", "of", "o", "and", "i", "n", "s", "e ", "r", " th", " t", "in", "he",
    "th", "h", "he ", "to", "\r\n", "l", "s ", "d", " a", "an", "er", "c", " o", "d ", "on", " of",
    "re", "of ", "t ", ", ", "is", "u", "at", "   ", "n ", "or", "which", "f", "m", "as", "it",
    "that", "\n", "was", "en", "  ", " w", "es", " an", " i", "\r", "f ", "g", "p", "nd", " s",
    "nd ", "ed ", "w", "ed", "http://", "for", "te", "ing", "y ", "The", " c", "ti", "r ", "his",
    "st", " in", "ar", "nt", ",", " to", "y", "ng", " h", "with", "le", "al", "to ", "b", "ou",
    "be", "were", " b", "se", "o ", "ent", "ha", "ng ", "their", "\"", "hi", "from", " f", "in ",
    "de", "ion", "me", "v", ".", "ve", "all", "re ", "ri", "ro", "is ", "co", "f t", "are", "ea",
    ". ", "her", " m", "er ", " p", "es ", "by", "they", "di", "ra", "ic", "not", "s, ", "d t",
    "at ", "ce", "la", "h ", "ne", "as ", "tio", "on ", "n t", "io", "we", " a ", "om", ", a",
    "s o", "ur", "li", "ll", "ch", "had", "this", "e t", "g ", "e\r\n", " wh", "ere", " co", "e o",
    "a ", "us", " d", "ss", "\n\r\n", "\r\n\r", "=\"", " be", " e", "s a", "ma", "one", "t t",
    "or ", "but", "el", "so", "l ", "e s", "s,", "no", "ter", " wa", "iv", "ho", "e a", " r",
    "hat", "s t", "ns", "ch ", "wh", "tr", "ut", "/", "have", "ly ", "ta", " ha", " on", "tha",
    "-", " l", "ati", "en ", "pe", " re", "there", "ass", "si", " fo", "wa", "ec", "our", "who",
    "its", "z", "fo", "rs", ">", "ot", "un", "<", "im", "th ", "nc", "ate", "><", "ver", "ad",
    " we", "ly", "ee", " n", "id", " cl", "ac", "il", "</", "rt", " wi", "div", "e, ", " it",
    "whi", " ma", "ge", "x", "e c", "men", ".com",
];

fn compress_bytes(input: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut verb = Vec::with_capacity(256);

    let mut pos = 0;

    while pos < input.len() {
        let mut j = 7.min(input.len() - pos);
        let mut matched = false;

        let h1 = (input[pos] as usize) << 3;

        let h2 = if j >= 2 {
            h1 + input[pos + 1] as usize
        } else {
            h1
        };

        let h3 = if j >= 3 {
            h2 ^ input[pos + 2] as usize
        } else {
            h2
        };

        while j > 0 {
            let hash = match j {
                1 => h1 % 241,
                2 => h2 % 241,
                _ => h3 % 241,
            };

            let slot = SMAZ_CB[hash];
            let mut offset = 0;

            while offset < slot.len() {
                let len = slot[offset] as usize;

                if len == 0 {
                    break;
                }

                if len == j
                    && offset + 1 + j < slot.len()
                    && slot[offset + 1..offset + 1 + j] == input[pos..pos + j]
                {
                    // Flush pending verbatim bytes.
                    if !verb.is_empty() {
                        flush_verbatim(&mut output, &verb);
                        verb.clear();
                    }

                    // Codebook output byte.
                    output.push(slot[offset + j + 1]);

                    pos += j;
                    matched = true;
                    break;
                }

                offset += len + 2;
            }

            if matched {
                break;
            }

            j -= 1;
        }

        if !matched {
            verb.push(input[pos]);
            pos += 1;

            if verb.len() == 256 || pos == input.len() {
                flush_verbatim(&mut output, &verb);
                verb.clear();
            }
        }
    }

    output
}

fn flush_verbatim(output: &mut Vec<u8>, verb: &[u8]) {
    if verb.len() == 1 {
        output.push(254);
        output.push(verb[0]);
    } else {
        output.push(255);
        output.push((verb.len() - 1) as u8);
        output.extend_from_slice(verb);
    }
}

fn decompress_bytes(input: &[u8]) -> Result<Vec<u8>, &'static str> {
    let mut output = Vec::new();
    let mut pos = 0;

    while pos < input.len() {
        match input[pos] {
            254 => {
                if pos + 1 >= input.len() {
                    return Err("truncated verbatim byte");
                }

                output.push(input[pos + 1]);
                pos += 2;
            }

            255 => {
                if pos + 1 >= input.len() {
                    return Err("truncated verbatim string");
                }

                let len = input[pos + 1] as usize + 1;

                if pos + 2 + len > input.len() {
                    return Err("truncated verbatim string");
                }

                output.extend_from_slice(&input[pos + 2..pos + 2 + len]);
                pos += 2 + len;
            }

            code => {
                let s = SMAZ_RCB
                    .get(code as usize)
                    .ok_or("invalid codebook entry")?;

                output.extend_from_slice(s.as_bytes());
                pos += 1;
            }
        }
    }

    Ok(output)
}

pub fn compress(input: &str) -> Vec<u8> {
    compress_bytes(input.as_bytes())
}

pub fn decompress(input: &[u8]) -> Result<String, &'static str> {
    let bytes = decompress_bytes(input)?;

    String::from_utf8(bytes).map_err(|_| "decompressed output is not valid UTF-8")
}
