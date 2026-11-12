# DomainGözcü

7/24 çalışan, tek binary'lik domain izleme servisi. İzlediğin domainlerin kayıt durumunu (aktif, süresi dolmuş, redemption, pendingDelete, boşta, satılık...) periyodik kontrol eder ve değişince **Discord** ve/veya **Telegram**'a bildirim gönderir.

![İzlenen domainler](docs/screenshots/list.png)

*Ekran görüntüleri gerçek çıktılardır: 7 domainlik bir demo veritabanıyla alındı. `gozcu-demo-7731-qx.com` kayıtsız bir test adıdır; `hugedomains.com` bir pazar yeri sitesi olduğu için `ParkSatilik` görünür.*

## Ne yapar, ne yapmaz

- Yapar: DNS / RDAP / WHOIS ile durum tespiti, durum değişikliğinde bildirim, bitiş tarihi uyarıları, günlük özet, durum geçmişi.
- **Yapmaz:** Domain satın almaz, kaydetmez, yenilemez; hiçbir registrar veya pazar yeri hesabına bağlanmaz. "Boşta" bildirimi geldiğinde kaydı kendin yaparsın.

## Nasıl tespit eder

- **NS (DNS)** ve **RDAP** (yoksa **WHOIS**) paralel sorgulanır. RDAP sunucusu IANA bootstrap'ten (`data.iana.org/rdap/dns.json`) bulunur; WHOIS sunucusu `whois.iana.org`'dan `refer:` ile bulunur.
- Durum RDAP/WHOIS status kodlarından çıkarılır. Park/satılık kontrolü yalnızca kayıtlı domainlerde yapılır (NS listesi, pazar yeri yönlendirmesi, sayfa metnindeki satılık ifadeleri).
- **"Boşta" çift doğrulanır:** İlk "kayıt yok" sonucunda bildirim gitmez; `bosta_dogrulama_dk` (varsayılan 2 dk) sonra tekrar bakılır. İkinci kontrol de kayıt yok diyorsa ve NS yoksa "Bosta" olur ve bildirim gider.
- **Geçici hatalar** (429, 5xx, timeout, ağ hatası) durumu değiştirmez; sadece backoff uygulanır. `ardisik_hata_uyarisi` kadar art arda hata olursa bir kez "kontrol edilemiyor" bildirimi gider.

Servis çalışırken geçişler loglanır ve her geçiş geçmişe yazılır:

![Servis logu](docs/screenshots/run.png)

Tek bir domain için karar akışını görmek için `check` kullanılır (veritabanına yazmaz, bildirim atmaz):

![check çıktısı](docs/screenshots/check.png)

Ayrıntılar: `docs/TESPIT_KURALLARI.md`, mimari: `docs/MIMARI.md`.

## Durumlar ve varsayılan kontrol aralıkları

| Durum | Anlamı | Aralık |
|---|---|---|
| `Aktif` | Kayıtlı, normal | 1440 dk |
| `ParkSatilik` | Kayıtlı, park/satılık izi var | 1440 dk |
| `BitisYakin` | Bitişe eşik günden az kaldı | 720 dk |
| `SuresiDoldu` | Bitiş tarihi geçmiş | 360 dk |
| `Redemption` | Redemption period | 60 dk |
| `PendingDelete` | Düşmek üzere | 10 dk |
| `Bosta` | Kayıt yok (2 kez doğrulandı) | 360 dk |
| `Askida` | Client/server hold | 360 dk |
| `Bilinmiyor` | Henüz kontrol edilmedi veya belirlenemedi | 30 dk |

Aralıklara `jitter_yuzde` (varsayılan ±%10) rastgelelik eklenir. Zamanlayıcı 15 saniyede bir vakti gelen domainleri tarar.

## Derleme ve test

```bash
cargo build --release    # target/release/domain-gozcu
cargo test               # 130 test; RDAP/WHOIS fixture'ları tests/fixtures/ altında
```

Binary glibc'ye dinamik bağlanır; derlendiği sistemden eski bir VDS'te çalışmayabilir. Statik (musl) derleme denenmedi.

## VDS'e kurulum (systemd)

Servis `domain-gozcu` adlı statik sistem kullanıcısıyla çalışır (`DynamicUser` kullanılmaz; CLI aynı kullanıcıyla çalışıp aynı veritabanına yazabilsin diye). Aşağıdakiler root olarak çalıştırılır:

```bash
# 1) Kullanıcı ve dizinler
useradd --system --no-create-home --shell /usr/sbin/nologin domain-gozcu
install -d -m 0755 /etc/domain-gozcu
install -d -o domain-gozcu -g domain-gozcu -m 0750 /var/lib/domain-gozcu

# 2) Binary
install -m 0755 target/release/domain-gozcu /usr/local/bin/domain-gozcu

# 3) Config (gizli bilgi içermez)
install -m 0644 config.example.toml /etc/domain-gozcu/config.toml
$EDITOR /etc/domain-gozcu/config.toml

# 4) Gizli bilgiler (sadece root okur)
install -m 0600 -o root -g root deploy/domain-gozcu.env.example /etc/domain-gozcu/domain-gozcu.env
$EDITOR /etc/domain-gozcu/domain-gozcu.env

# 5) Unit
install -m 0644 deploy/domain-gozcu.service /etc/systemd/system/domain-gozcu.service
systemctl daemon-reload
systemctl enable --now domain-gozcu

# 6) Log
journalctl -u domain-gozcu -f
```

Notlar:

- `config.example.toml` Discord ve Telegram'ı `aktif = true` bırakır. Gizli bilgi yoksa servis **başlamaz**. Kullanmayacağın kanalı `aktif = false` yap ya da env dosyasını doldur.
- Önce `test-notify` ile bildirimleri doğrula, sonra `enable --now` kullan.
- Unit sandbox'lıdır (`ProtectSystem=strict`, `NoNewPrivileges`, syscall filtresi vb.); yazma sadece `/var/lib/domain-gozcu` altındadır. Bu sertleştirme bu makinede gerçek kurulumla test edilmedi; ilk başlatmada `journalctl` çıktısına bak.
- Durdurmada daemon işleri en fazla 30 sn bekler (`TimeoutStopSec=45`).
- Güncelleme: yeni binary'yi `install` ile kopyala, `systemctl restart domain-gozcu`.

## CLI

```
domain-gozcu [-c|--config <dosya>] <komut>
```

`-c` varsayılanı `/etc/domain-gozcu/config.toml`. `-c` global bayraktır; komuttan önce ya da sonra yazılabilir.

CLI ve servis aynı SQLite veritabanını (WAL modu) paylaşır; `add`, `remove` ve `import` servisi yeniden başlatmadan etki eder. Veritabanı dosyaları `domain-gozcu` kullanıcısına aittir. Bu yüzden komutları bu kullanıcıyla çalıştır; root ile `add` yaparsan oluşan dosyalar root'a ait olur ve servis yazamaz.

![Komut listesi](docs/screenshots/help.png)

```bash
# Servisi başlatır (systemd bunu çağırır)
domain-gozcu run

# Ekle / sil (--not ve --etiket isteğe bağlı; IDN girdileri punycode'a çevrilir)
sudo -u domain-gozcu domain-gozcu add ornek.com --not "Düşerse haber ver" --etiket hedef
sudo -u domain-gozcu domain-gozcu remove ornek.com

# Listele; --durum ile filtrele (Aktif, ParkSatilik, BitisYakin, SuresiDoldu, Redemption, PendingDelete, Bosta, Askida, Bilinmiyor)
sudo -u domain-gozcu domain-gozcu list
sudo -u domain-gozcu domain-gozcu list --durum PendingDelete

# Tek seferlik kontrol: ham sonuç ve durum makinesinin kararı (veritabanına yazmaz, bildirim atmaz)
sudo -u domain-gozcu domain-gozcu check ornek.com

# Dosyadan toplu ekle: satır başına bir domain; boş satırlar ve # ile başlayanlar atlanır
sudo -u domain-gozcu domain-gozcu import /var/lib/domain-gozcu/domainler.txt

# Durum geçmişi
sudo -u domain-gozcu domain-gozcu history ornek.com

# Etkin kanallara test mesajı (kuyruğu atlar; hata varsa çıkış kodu sıfır değildir)
sudo sh -c 'set -a; . /etc/domain-gozcu/domain-gozcu.env; exec domain-gozcu test-notify'
```

`test-notify` veritabanına dokunmaz. Webhook ve token sadece `run` ve `test-notify` için gerekir; diğer komutlar gizli bilgi olmadan çalışır.

`config.toml` içindeki `[[domain]]` girdileri her `run` başlangıcında ve CLI komutlarında veritabanıyla eşitlenir.

## Yapılandırma

Tüm anahtarlar isteğe bağlıdır; verilmeyenler aşağıdaki varsayılanları alır. Örnek: `config.example.toml`.

### `[genel]`

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `veritabani` | `/var/lib/domain-gozcu/gozcu.db` | SQLite dosyası. Üst dizin yoksa oluşturulmaya çalışılır. `rdap-dns.json` önbelleği aynı dizine yazılır. |
| `log_seviyesi` | `info` | `RUST_LOG` tanımlıysa o öncelik alır. |
| `eszamanli_kontrol` | `8` | Aynı anda kontrol edilen en fazla domain (> 0). |
| `sunucu_istek_per_sn` | `1` | Her RDAP/WHOIS sunucusuna saniyede en fazla istek (> 0). |
| `http_timeout_sn` | `10` | HTTP zaman aşımı (> 0). |
| `kullanici_ajani` | `DomainGozcu/1.0 (+kişisel izleme aracı)` | HTTP User-Agent. |

### `[zamanlama]` (dakika, hepsi > 0)

`aktif` 1440, `park_satilik` 1440, `bitis_yakin` 720, `suresi_doldu` 360, `redemption` 60, `pending_delete` 10, `bosta` 360, `askida` 360, `bilinmiyor` 30, `jitter_yuzde` 10 (0-100), `bosta_dogrulama_dk` 2.

### `[uyarilar]`

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `bitis_esikleri_gun` | `[30, 7, 1]` | Bitişe bu kadar gün kalınca uyarı; her eşik bir kez bildirilir. |
| `pending_delete_gun` | `5` | pendingDelete düşme tahmini için gün sayısı. |
| `ardisik_hata_uyarisi` | `5` | Bu kadar ardışık hatada bir kez "kontrol edilemiyor" bildirimi. |
| `gunluk_ozet` | `true` | Günlük özet gönderilsin mi. |
| `gunluk_ozet_saat` | `"09:00"` | `SS:DD`, `saat_dilimi`'ne göre. |
| `saat_dilimi` | `"Europe/Istanbul"` | IANA saat dilimi. |

`[uyarilar.uzanti_pending_delete_gun]` ile uzantıya göre `pending_delete_gun` değeri ezilebilir (ör. `org = 5`).

### `[bildirim.discord]`

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `aktif` | `false` (örnekte `true`) | Açıksa webhook zorunlu. |
| `webhook_url` | boş | Config'e yazma; `DOMAIN_GOZCU_DISCORD_WEBHOOK` kullan. Env değişkeni boş değilse config'deki değeri geçersiz kılar. |
| `bosta_mention` | boş (örnekte `"@everyone"`) | Sadece "boşta" mesajına eklenir. Rol için `"<@&ROL_ID>"`. |

### `[bildirim.telegram]`

| Anahtar | Varsayılan | Anlamı |
|---|---|---|
| `aktif` | `false` (örnekte `true`) | Açıksa `bot_token` ve `chat_id` zorunlu. |
| `bot_token` | boş | Config'e yazma; `DOMAIN_GOZCU_TELEGRAM_TOKEN` kullan. |
| `chat_id` | yok | Metin olarak yaz: `"123456789"`; grup için `"-100..."`. |

### `[tespit]` (liste verilirse varsayılanı tamamen değiştirir)

| Anahtar | Anlamı |
|---|---|
| `park_ns` | NS'in sonu bunlardan biriyle bitiyorsa park sayılır. |
| `market_alanlari` | Site bu alanlara yönleniyorsa satılık sayılır. |
| `satilik_ifadeleri` | Sayfa metninde (büyük/küçük harf duyarsız) aranır. |
| `whois_bulunamadi_ifadeleri` | WHOIS yanıtında bulunursa "kayıt yok" sayılır. |

Varsayılan listeleri `config.example.toml` içinde bulabilirsin.

### `[[domain]]` (başlangıç listesi, isteğe bağlı)

```toml
[[domain]]
ad = "ornek.com"
not = "Düşerse haber ver"
etiket = ["hedef"]
```

`ad` zorunludur. `not` ve `etiket` isteğe bağlıdır.

## Bildirimler

| Olay | Başlık |
|---|---|
| Boşta (2 kez doğrulandı) | 🟢 `ornek.com BOŞTA görünüyor.` (yüksek öncelik; `bosta_mention` eklenir) |
| pendingDelete | 🔴 `pendingDelete!` + tahmini düşme tarihi |
| Redemption'a girdi | 🟠 `redemption dönemine girdi.` |
| Satılık / park | 🟣 `satılık görünüyor.` |
| Diğer durum değişimleri | `durumu değişti: X → Y.` (durum emojisiyle) |
| Bitiş uyarısı | ⏳ `bitişine N gün kaldı.` |
| Yenilendi | 🔄 `yenilendi.` + yeni bitiş |
| Başkası aldı | ⚪ `tekrar alındı.` + yeni registrar |
| Kontrol edilemiyor | ⚠️ `kontrol edilemiyor.` |
| Günlük özet | 📋 `Günlük özet — YYYY-AA-GG` |

- Bildirimler önce SQLite kuyruğuna yazılır ve ayrı bir işçi gönderir; servis yeniden başlasa da kaybolmaz.
- Gönderim başarısız olursa üstel backoff ile tekrar denenir: `30 sn × 2^deneme`, en çok 1 saat, ±%20 jitter. HTTP 429 alınırsa sunucunun `Retry-After` süresi beklenir ve bu deneme sayılmaz.
- `bosta_mention` yalnızca Discord'da ve yalnızca "boşta" mesajında kullanılır.

### Discord webhook

1. Sunucu Ayarları → **Entegrasyonlar** → **Webhooklar** → **Yeni Webhook**.
2. Kanalı seç, **Webhook URL'sini Kopyala**.
3. `/etc/domain-gozcu/domain-gozcu.env` içine `DOMAIN_GOZCU_DISCORD_WEBHOOK=...` olarak yaz.

### Telegram bot

1. @BotFather ile `/newbot` komutunu çalıştır; verilen token'ı `DOMAIN_GOZCU_TELEGRAM_TOKEN=...` olarak env dosyasına yaz.
2. Bota bir mesaj gönder (grup kullanacaksan botu gruba ekle ve grupta bir mesaj yaz).
3. `https://api.telegram.org/bot<TOKEN>/getUpdates` adresindeki `message.chat.id` değeri `chat_id`'dir. Bireysel sohbet id'leri pozitif, grup id'leri negatiftir.

**Gizli bilgiler (webhook URL, bot token) sadece env dosyasında durur; config'e, repoya veya loglara yazılmaz.**

## Bilinen sınırlar

- `.tr`: WHOIS (`whois.trabis.gov.tr`) çalışır; registrar ve son değişiklik tarihi parse edilmez.
- DENIC `.de` gibi özel WHOIS sorgu sözdizimi isteyen uzantılar desteklenmez.
- WHOIS "kayıt yok" ifadeleri registry'ye göre değişir; yanlış sonuç görürsen `whois_bulunamadi_ifadeleri` listesini gerçek çıktıya göre güncelle.
- Park NS listesi ve satılık ifadeleri başlangıç listeleridir; gerçek domainlerle doğrula. `domaincontrol.com` gibi genel NS'ler bilerek park sayılmaz.
- `Retry-After` başlığının HTTP-date biçimi desteklenmez; yalnızca saniye değeri okunur.
- pendingDelete düşme tarihi bir **tahmindir** (`pending_delete_gun`); registry'nin gerçek silme anı farklı olabilir.
- RDAP/WHOIS sunucuları hız sınırlar; `sunucu_istek_per_sn` varsayılanı (1) bilerek düşüktür.

## Lisans

[MIT](LICENSE)
