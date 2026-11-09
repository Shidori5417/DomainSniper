# DomainGözcü — Geliştirme Promptu

> Bu promptu bir AI kodlama asistanına (Claude Code vb.) ver. Aynı klasördeki `docs/MIMARI.md`, `docs/TESPIT_KURALLARI.md` ve `config.example.toml` dosyalarını da projeye koy ve asistana okumasını söyle.

---

## Görev

Rust ile **DomainGözcü** adında, bir Linux VDS sunucusunda 7/24 çalışacak bir **domain izleme servisi** yaz. Servis, kullanıcının listesindeki domainlerin durumunu düzenli aralıklarla kontrol eder ve **durum değiştiğinde** Discord ve/veya Telegram'a bildirim gönderir.

**Kapsam dışı (kesinlikle yapılmayacak):** Domain satın alma, otomatik kayıt, registrar API'si ile işlem, ödeme. Bu araç **yalnızca izler ve bildirir.**

Başlamadan önce `docs/MIMARI.md` ve `docs/TESPIT_KURALLARI.md` dosyalarını baştan sona oku. Tespit mantığı ve durum makinesi oradaki kurallara birebir uymalı.

## Teknik gereksinimler

- **Dil:** Rust (stable, edition 2021 veya üstü). Python veya başka dil yok.
- **Çıktı:** Tek bir statik binary (`domain-gozcu`). Mümkünse `x86_64-unknown-linux-musl` ile derlenebilir olsun.
- **Önerilen crate'ler** (daha iyi bir gerekçen varsa değiştirebilirsin, ama gerekçeyi yaz):
  - `tokio` (multi-thread runtime), `reqwest` (rustls, json, gzip — OpenSSL bağımlılığı olmasın)
  - `hickory-resolver` (DNS / NS sorguları)
  - `rusqlite` (bundled) — durum ve geçmiş için
  - `serde`, `serde_json`, `toml` — config ve RDAP parse
  - `clap` (derive) — CLI
  - `tracing`, `tracing-subscriber` — loglama
  - `governor` — sunucu bazlı rate limit
  - `chrono` veya `time` — tarih hesapları
  - `thiserror` / `anyhow` — hata yönetimi
- **Performans:** Hafif olmalı. Boşta neredeyse sıfır CPU, düşük RAM (hedef: 1000 domain izlerken < 30 MB). Eşzamanlılık `Semaphore` ile sınırlansın, istekler aynı anda patlamasın (jitter ekle).

## Özellikler

### 1. Domain kontrolü
Her domain için `docs/TESPIT_KURALLARI.md`'deki sırayla:
1. **DNS ön kontrol:** NS kaydı var mı?
2. **RDAP sorgusu:** IANA bootstrap (`https://data.iana.org/rdap/dns.json`) ile doğru RDAP sunucusunu bul, durum kodlarını ve tarihleri (kayıt, bitiş, son değişiklik) çek. Bootstrap dosyasını günde bir yenile, diskte cache'le.
3. **WHOIS yedeği:** Uzantının RDAP sunucusu yoksa port 43 WHOIS'e düş. WHOIS sunucusunu hardcode etme; `whois.iana.org` referral'ından bul ve cache'le.
4. **Satılık/park tespiti:** NS'ler bilinen park/market sunucularına mı gidiyor, HTTP sayfasında satılık ifadeleri var mı, bilinen marketlere redirect var mı.

Sonuç olarak her domain tek bir **ana duruma** oturtulur (bkz. `docs/MIMARI.md` → Durum makinesi).

### 2. Bildirimler
- **Discord webhook** (embed formatında, duruma göre renkli) ve **Telegram Bot API** (`sendMessage`, HTML parse_mode). Config'den ikisi ayrı ayrı açılıp kapatılabilir.
- **Sadece durum değişince** bildirim at, aynı durumu tekrar tekrar bildirme.
- Ek bildirimler:
  - Bitişe yaklaşma uyarıları (config'den eşikler, varsayılan: 30, 7, 1 gün kala). Her eşik bir kez bildirilir.
  - `pendingDelete`'e girince **tahmini düşme tarihi** (giriş + ~5 gün; uzantıya göre config'den ayarlanabilir).
  - Domain **boşa düştüğünde / kayıt edilebilir göründüğünde** yüksek öncelikli bildirim (Discord'da opsiyonel `@everyone` / rol mention, config'den).
  - Opsiyonel **günlük özet** (config'den saat ayarlanır).
- Discord 429 dönerse `retry_after` kadar bekle. Telegram 429'da `retry_after` parametresine uy. Bildirim gönderilemezse kuyrukta tut ve tekrar dene, kaybolmasın.

### 3. Akıllı kontrol sıklığı
Her domainin kontrol aralığı durumuna göre değişir (config'den ayarlanabilir, varsayılanlar `config.example.toml`'da). Örnek: normal aktif domain 24 saatte bir, redemption'daki 1 saatte bir, pendingDelete'teki 10 dakikada bir. Zamanlama **SQLite'taki `next_check_at` alanına göre** yapılsın, servis yeniden başlasa da kaldığı yerden devam etsin.

### 4. CLI
```
domain-gozcu run                      # servisi başlatır (daemon modu, systemd için)
domain-gozcu add ornek.com [--not "..."] [--etiket ...]
domain-gozcu remove ornek.com
domain-gozcu list [--durum ...]       # tablo halinde: domain, durum, bitiş, sonraki kontrol
domain-gozcu check ornek.com          # tek seferlik kontrol, sonucu detaylı yazdırır, bildirim atmaz
domain-gozcu import domains.txt       # satır satır domain listesi
domain-gozcu history ornek.com        # durum değişiklik geçmişi
domain-gozcu test-notify              # açık olan tüm kanallara test mesajı yollar
```
`add`/`remove` çalışan servisi yeniden başlatmadan etki etsin (servis SQLite'ı periyodik okur ya da yeni kayıtları fark eder).

Domain girişi normalize edilsin: küçük harf, `https://`, `www.` ve sondaki `/` temizlensin. Türkçe karakterli (IDN) domainler punycode'a çevrilsin (`idna` crate) ama listede okunur hali gösterilsin.

### 5. Dayanıklılık
- Rate limit: RDAP/WHOIS sunucusu bazında `governor` ile. 429 veya bağlantı hatasında exponential backoff + jitter.
- Geçici hata (timeout, 5xx) **asla durum değişikliği sayılmasın.** Arka arkaya N hata olursa (config) "kontrol edilemiyor" uyarısı bir kez bildirilsin.
- **"Boşta" kararı için çift doğrulama:** RDAP 404 + NS yok sonucu, kısa aralıkla ikinci kez teyit edilmeden "boşa düştü" bildirimi atılmasın (yanlış alarm olmasın).
- SIGTERM/SIGINT'te düzgün kapanış (yarım kalan yazmaları bitir).

### 6. Teslim edilecekler
- Çalışan Rust projesi (`cargo build --release` ile derlenir, `cargo clippy` uyarısız).
- `config.example.toml` (verilen örneğe uygun).
- `deploy/domain-gozcu.service` — systemd unit dosyası (ayrı kullanıcı, `Restart=always`, sandboxing seçenekleri: `ProtectSystem=strict`, `NoNewPrivileges=true` vb.).
- `README.md` (Türkçe): kurulum, VDS'e deploy, config açıklaması, Discord webhook ve Telegram bot token'ı nasıl alınır.
- **Testler:** RDAP JSON ve WHOIS çıktısı parser'ları için gerçekçi fixture dosyalarıyla unit testler. Durum makinesi geçişleri için testler. Tespit kuralları (park NS, satılık anahtar kelimeleri) için testler.

## Çalışma şekli
1. Önce proje iskeletini ve modül yapısını (`docs/MIMARI.md`'deki gibi) kur, bana göster.
2. Sonra sırayla: config + DB → RDAP/WHOIS/DNS istemcileri → tespit ve durum makinesi → zamanlayıcı → bildirimler → CLI → systemd/README.
3. Her aşamadan sonra derlenebilir ve test edilebilir durumda bırak.
4. Emin olmadığın bir registry davranışı olursa (ör. bir uzantının RDAP formatı) varsayım yapıp gömme. Config'den ayarlanabilir yap ve README'de not düş.
