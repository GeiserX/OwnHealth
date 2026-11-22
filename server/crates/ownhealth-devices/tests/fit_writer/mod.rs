//! A small FIT writer for the tests and the fixtures in `fixtures/fit`: the file layout of the public
//! FIT protocol (a 14-byte header, definition and data messages with normal headers, little endian, and
//! the CRC-16 the protocol documents), with the message and field numbers of the public FIT profile.
//! It writes only what the importer reads, plus positions, so the no-position checks mean something.
//! Synthetic activities of Lucía Rivera; no file here comes from a device, a vendor SDK or a person.
#![allow(dead_code)]

/// 1989-12-31 00:00 UTC, the FIT epoch, in unix seconds.
pub const FIT_EPOCH: i64 = 631_065_600;

// base types
pub const ENUM: u8 = 0x00;
pub const UINT8: u8 = 0x02;
pub const STRING: u8 = 0x07;
pub const UINT16: u8 = 0x84;
pub const SINT32: u8 = 0x85;
pub const UINT32: u8 = 0x86;
pub const UINT32Z: u8 = 0x8C;

// global message numbers
pub const FILE_ID: u16 = 0;
pub const SESSION: u16 = 18;
pub const RECORD: u16 = 20;
pub const ACTIVITY: u16 = 34;

// sports
pub const RUNNING: u8 = 1;
pub const CYCLING: u8 = 2;
pub const SWIMMING: u8 = 5;

/// Where the synthetic runs start, in degrees: a park, not anyone's home.
pub const LAT0: f64 = 40.4153;
pub const LON0: f64 = -3.6845;

const CRC_TABLE: [u16; 16] = [
    0x0000, 0xCC01, 0xD801, 0x1400, 0xF001, 0x3C00, 0x2800, 0xE401, 0xA001, 0x6C00, 0x7800, 0xB401, 0x5000, 0x9C01,
    0x8801, 0x4400,
];

/// The FIT CRC-16 of `bytes`, starting from `crc`.
pub fn crc(mut crc: u16, bytes: &[u8]) -> u16 {
    for b in bytes {
        let mut t = CRC_TABLE[usize::from(crc & 0xF)];
        crc = (crc >> 4) & 0x0FFF;
        crc = crc ^ t ^ CRC_TABLE[usize::from(b & 0xF)];
        t = CRC_TABLE[usize::from(crc & 0xF)];
        crc = (crc >> 4) & 0x0FFF;
        crc = crc ^ t ^ CRC_TABLE[usize::from((b >> 4) & 0xF)];
    }
    crc
}

/// FIT time of a unix time.
pub fn fit_time(unix: i64) -> u32 {
    u32::try_from(unix - FIT_EPOCH).unwrap_or_default()
}

/// Degrees as FIT semicircles.
pub fn semicircles(deg: f64) -> i32 {
    (deg * 2f64.powi(31) / 180.0).round() as i32
}

/// A field value as its bytes.
pub enum V<'a> {
    U8(u8),
    U16(u16),
    U32(u32),
    I32(i32),
    Str(&'a str, usize),
}

impl V<'_> {
    fn bytes(&self) -> Vec<u8> {
        match self {
            V::U8(v) => vec![*v],
            V::U16(v) => v.to_le_bytes().to_vec(),
            V::U32(v) => v.to_le_bytes().to_vec(),
            V::I32(v) => v.to_le_bytes().to_vec(),
            V::Str(s, n) => {
                let mut b = s.as_bytes().to_vec();
                b.resize(*n, 0);
                b
            }
        }
    }
}

#[derive(Default)]
pub struct Writer {
    body: Vec<u8>,
}

impl Writer {
    /// A definition message: local type, global message number, fields as (number, size, base type).
    pub fn define(&mut self, local: u8, global: u16, fields: &[(u8, u8, u8)]) -> &mut Self {
        self.body.extend([0x40 | local, 0, 0]);
        self.body.extend(global.to_le_bytes());
        self.body.push(u8::try_from(fields.len()).unwrap_or_default());
        for (n, size, base) in fields {
            self.body.extend([*n, *size, *base]);
        }
        self
    }

    /// A data message of a defined local type, its values in the definition's order.
    pub fn data(&mut self, local: u8, values: &[V]) -> &mut Self {
        self.body.push(local);
        for v in values {
            self.body.extend(v.bytes());
        }
        self
    }

    /// The file: a 14-byte header whose own CRC is 0 (so the file CRC covers it, as the protocol
    /// allows), the messages, and the file CRC.
    pub fn finish(&self) -> Vec<u8> {
        let mut out = vec![14u8, 0x20];
        out.extend(2132u16.to_le_bytes());
        out.extend(u32::try_from(self.body.len()).unwrap_or_default().to_le_bytes());
        out.extend(b".FIT");
        out.extend([0, 0]);
        out.extend(&self.body);
        let c = crc(0, &out);
        out.extend(c.to_le_bytes());
        out
    }
}

/// One synthetic activity.
pub struct Activity {
    pub start: i64,
    pub seconds: u32,
    /// Seconds between two records.
    pub every: u32,
    pub sport: u8,
    pub gps: bool,
    pub distance_m: u32,
    pub kcal: u16,
    pub serial: u32,
    pub product_name: &'static str,
}

/// The heart rate the synthetic watch reads at second `s`: within minute `m` the readings 130, 132, 134
/// and 136 repeat, plus `m`, so each full minute's mean is `133 + m` when the readings are evenly spread.
pub fn hr_at(s: u32, every: u32) -> u8 {
    let k = (s / every) % 4;
    u8::try_from(130 + 2 * k + s / 60).unwrap_or(u8::MAX)
}

/// file_id, `records`, one session and the activity message.
pub fn activity(a: &Activity) -> Vec<u8> {
    let mut w = Writer::default();
    file_id(&mut w, 4, a.serial, a.product_name, a.start);
    let mut rec = vec![(253, 4, UINT32), (3, 1, UINT8), (5, 4, UINT32)];
    if a.gps {
        rec.extend([(0, 4, SINT32), (1, 4, SINT32)]);
    }
    w.define(1, RECORD, &rec);
    let mut s = 0;
    while s < a.seconds {
        let t = fit_time(a.start + i64::from(s));
        let dist = u32::try_from(u64::from(a.distance_m) * u64::from(s) * 100 / u64::from(a.seconds)).unwrap_or(0);
        let mut vals = vec![V::U32(t), V::U8(hr_at(s, a.every)), V::U32(dist)];
        if a.gps {
            vals.push(V::I32(semicircles(LAT0 + f64::from(s) * 1e-5)));
            vals.push(V::I32(semicircles(LON0 + f64::from(s) * 1e-5)));
        }
        w.data(1, &vals);
        s += a.every;
    }
    let end = fit_time(a.start + i64::from(a.seconds));
    let mut ses = vec![
        (253, 4, UINT32),
        (2, 4, UINT32),
        (5, 1, ENUM),
        (7, 4, UINT32),
        (8, 4, UINT32),
        (9, 4, UINT32),
        (11, 2, UINT16),
    ];
    if a.gps {
        ses.extend([
            (3, 4, SINT32),
            (4, 4, SINT32),
            (29, 4, SINT32),
            (30, 4, SINT32),
            (31, 4, SINT32),
            (32, 4, SINT32),
        ]);
    }
    w.define(2, SESSION, &ses);
    let mut vals = vec![
        V::U32(end),
        V::U32(fit_time(a.start)),
        V::U8(a.sport),
        V::U32(a.seconds * 1000),
        V::U32(a.seconds * 1000),
        V::U32(a.distance_m * 100),
        V::U16(a.kcal),
    ];
    if a.gps {
        let far = f64::from(a.seconds) * 1e-5;
        vals.extend([
            V::I32(semicircles(LAT0)),
            V::I32(semicircles(LON0)),
            V::I32(semicircles(LAT0 + far)),
            V::I32(semicircles(LON0 + far)),
            V::I32(semicircles(LAT0)),
            V::I32(semicircles(LON0)),
        ]);
    }
    w.data(2, &vals);
    w.define(
        3,
        ACTIVITY,
        &[
            (253, 4, UINT32),
            (1, 2, UINT16),
            (2, 1, ENUM),
            (3, 1, ENUM),
            (4, 1, ENUM),
            (5, 4, UINT32),
        ],
    );
    // local time one hour ahead of UTC, as a watch in Madrid in winter writes it
    w.data(
        3,
        &[
            V::U32(end),
            V::U16(1),
            V::U8(0),
            V::U8(26),
            V::U8(1),
            V::U32(end + 3600),
        ],
    );
    w.finish()
}

/// A file_id message of a FIT `type` (4 activity, 6 course) from the synthetic watch.
pub fn file_id(w: &mut Writer, kind: u8, serial: u32, product_name: &str, created: i64) {
    w.define(
        0,
        FILE_ID,
        &[
            (0, 1, ENUM),
            (1, 2, UINT16),
            (2, 2, UINT16),
            (3, 4, UINT32Z),
            (4, 4, UINT32),
            (8, 24, STRING),
        ],
    );
    // manufacturer 255 is "development" in the FIT profile
    w.data(
        0,
        &[
            V::U8(kind),
            V::U16(255),
            V::U16(1),
            V::U32(serial),
            V::U32(fit_time(created)),
            V::Str(product_name, 24),
        ],
    );
}

/// 2026-11-14T08:00:00Z and 2026-11-15T17:30:00Z.
pub const RUN_START: i64 = 1_794_643_200;
pub const RIDE_START: i64 = 1_794_763_800;
pub const WATCH: &str = "OwnHealth Test Watch";
pub const SERIAL: u32 = 3_001_029;

/// Lucía's 30-minute run with GPS: a record every 5 s, 5.23 km, 312 kcal.
pub fn run_with_gps() -> Vec<u8> {
    activity(&Activity {
        start: RUN_START,
        seconds: 1800,
        every: 5,
        sport: RUNNING,
        gps: true,
        distance_m: 5230,
        kcal: 312,
        serial: SERIAL,
        product_name: WATCH,
    })
}

/// Lucía's 20-minute bike ride, no GPS: a record every 10 s, 8.4 km, 210 kcal.
pub fn ride() -> Vec<u8> {
    activity(&Activity {
        start: RIDE_START,
        seconds: 1200,
        every: 10,
        sport: CYCLING,
        gps: false,
        distance_m: 8400,
        kcal: 210,
        serial: SERIAL,
        product_name: WATCH,
    })
}

/// A course (a route to follow): positions only, no heart rate, distance or energy.
pub fn course() -> Vec<u8> {
    let mut w = Writer::default();
    file_id(&mut w, 6, SERIAL, WATCH, RUN_START);
    w.define(1, RECORD, &[(253, 4, UINT32), (0, 4, SINT32), (1, 4, SINT32)]);
    for i in 0..20i64 {
        w.data(
            1,
            &[
                V::U32(fit_time(RUN_START + i)),
                V::I32(semicircles(LAT0 + i as f64 * 1e-4)),
                V::I32(semicircles(LON0)),
            ],
        );
    }
    w.finish()
}

/// gzip of `bytes`, with no name and no time in its header.
pub fn gzip(bytes: &[u8]) -> Vec<u8> {
    use std::io::Write as _;
    let mut e = flate2::GzBuilder::new()
        .mtime(0)
        .write(Vec::new(), flate2::Compression::default());
    e.write_all(bytes).ok();
    e.finish().unwrap_or_default()
}
