# DomainGözcü — Mimari

## Genel akış

```
                 ┌────────────────────┐
  CLI (add/...)──▶      SQLite        ◀───────────────┐
                 │ domains / history  │                │
                 └─────────┬──────────┘                │
                           │ next_check_at <= now      │ yeni durum + geçmiş
                    ┌──────▼───────┐                   │
                    │  Zamanlayıcı │ (her ~15 sn tarar) │
                    └──────┬───────┘                   │
                           │ iş (Semaphore ile sınırlı)│
             ┌─────────────▼──────────────┐            │
             │        Kontrol işçisi      │            │
             │ DNS → RDAP/WHOIS → HTTP    │            │
             └─────────────┬──────────────┘            │
                           │ ham sonuç                 │
                    ┌──────▼───────┐                   │
                    │ Durum makinesi├──────────────────┘
                    └──────┬───────┘
                           │ geçiş olduysa olay
                    ┌──────▼───────┐
                    │ Bildirim     │──▶ Discord webhook
                    │ kuyruğu      │──▶ Telegram Bot API
                    └──────────────┘
```

## Modül yapısı

```
src/
  main.rs            # clap, alt komutlara yönlendirme
  config.rs          # TOML okuma + doğrulama
  db.rs              # rusqlite, migration'lar, sorgular
  domain.rs          # normalize, IDN/punycode
  scheduler.rs       # next_check_at taraması, iş dağıtımı, jitter
  check/
    mod.rs           # tek domain kontrol akışı (sırayı burası yönetir)
    dns.rs           # NS / A kayıtları (hickory-resolver)
    rdap.rs          # bootstrap cache + sorgu + parse
    whois.rs         # port 43, iana referral, ham metin parse
    parking.rs       # park NS eşleşmesi + HTTP satılık tespiti
  state.rs           # Durum enum'u, geçiş kuralları, aralık hesabı
  notify/
    mod.rs           # olay tipleri, kuyruk, retry
    discord.rs
    telegram.rs
  ratelimit.rs       # sunucu bazlı governor limiter'ları
```

## Durum makinesi

| Durum | Anlamı | Varsayılan kontrol aralığı |
|---|---|---|
| `Aktif` | Kayıtlı, kullanılıyor (site/DNS çalışıyor) | 24 saat |
| `ParkSatilik` | Kayıtlı ama park edilmiş / satışa konmuş | 24 saat |
| `BitisYakin` | Bitişe ≤ 30 gün (eşik config'den) | 12 saat |
| `SuresiDoldu` | Bitiş tarihi geçti, auto-renew grace döneminde | 6 saat |
| `Redemption` | RDAP/WHOIS `redemption period` | 1 saat |
| `PendingDelete` | `pending delete`, düşme yakın | 10 dakika |
| `Bosta` | Kayıtlı değil, kaydedilebilir görünüyor (çift doğrulamalı) | 6 saat (tekrar alınırsa yakalamak için) |
| `Askida` | `client hold` / `server hold`, DNS'ten düşürülmüş | 6 saat |
| `Bilinmiyor` | Henüz kontrol edilmedi / sürekli hata | 30 dakika |

Önemli kurallar:
- Geçici hata **durum değiştirmez**. Sadece `hata_sayaci` artar.
- `Aktif` ile `ParkSatilik` arası geçiş de bildirilir ("artık satılık görünüyor" önemli bir bilgi).
- `Bosta` → herhangi bir kayıtlı durum geçişi = "domain biri tarafından alındı" bildirimi.
- `PendingDelete`'e ilk girişte `pending_delete_since` kaydedilir. Tahmini düşme = bu tarih + `pending_delete_gun` (varsayılan 5).

## Veritabanı şeması (öneri)

```sql
CREATE TABLE domains (
  id                    INTEGER PRIMARY KEY,
  name                  TEXT NOT NULL UNIQUE,   -- punycode
  display_name          TEXT NOT NULL,          -- okunur hali
  note                  TEXT,
  tags                  TEXT,
  state                 TEXT NOT NULL DEFAULT 'Bilinmiyor',
  registrar             TEXT,
  created_at_reg        TEXT,                   -- registry kayıt tarihi
  expires_at            TEXT,
  pending_delete_since  TEXT,
  last_status_codes     TEXT,                   -- JSON dizi
  last_nameservers      TEXT,                   -- JSON dizi
  last_checked_at       TEXT,
  next_check_at         TEXT NOT NULL,
  error_count           INTEGER NOT NULL DEFAULT 0,
  notified_thresholds   TEXT,                   -- JSON: [30,7] gibi, tekrar bildirmemek için
  pending_free_confirm  INTEGER NOT NULL DEFAULT 0 -- "boşta" çift doğrulama bayrağı
);

CREATE TABLE history (
  id         INTEGER PRIMARY KEY,
  domain_id  INTEGER NOT NULL REFERENCES domains(id) ON DELETE CASCADE,
  at         TEXT NOT NULL,
  from_state TEXT,
  to_state   TEXT NOT NULL,
  detail     TEXT
);

CREATE TABLE notify_queue (
  id         INTEGER PRIMARY KEY,
  channel    TEXT NOT NULL,     -- discord / telegram
  payload    TEXT NOT NULL,
  attempts   INTEGER NOT NULL DEFAULT 0,
  next_try   TEXT NOT NULL
);
```

WAL modu açık olmalı (`PRAGMA journal_mode=WAL`). Böylece CLI ile servis aynı anda DB'ye erişebilir.

## Bildirim örnekleri

- 🟠 **ornek.com redemption dönemine girdi.** Bitiş: 2026-10-01. Registrar: X
- 🔴 **ornek.com pendingDelete!** Tahmini düşme: 2026-11-14 (~5 gün)
- 🟢 **ornek.com BOŞTA görünüyor.** Kaydedilebilir (2 kez doğrulandı)
- 🟣 **ornek.com satılık görünüyor.** NS: ns1.sedoparking.com
- ⚪ **ornek.com tekrar alındı.** Yeni registrar: Y
- ⏳ **ornek.com bitişine 7 gün kaldı.**

Discord'da embed rengi duruma göre, Telegram'da emoji ve kalın başlık kullanılır.
