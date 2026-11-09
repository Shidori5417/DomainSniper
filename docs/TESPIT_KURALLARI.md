# DomainGözcü — Tespit Kuralları

## 1. Kontrol sırası

1. **DNS:** NS kayıtlarını sorgula.
   - NS yoksa: domain düşmüş, askıya alınmış ya da hiç kayıtlı olmayabilir. Yine de RDAP'a git (tek başına karar verme).
2. **RDAP:**
   - Bootstrap: `https://data.iana.org/rdap/dns.json` → TLD için base URL → `{base}domain/{ad}`.
   - **HTTP 200:** Kayıtlı. `status` dizisini ve `events` (`registration`, `expiration`, `last changed`) alanlarını oku. Registrar'ı `entities` içindeki `registrar` rolünden al.
   - **HTTP 404:** Registry'de kayıt yok, büyük ihtimalle boşta. Çift doğrulamaya gönder.
   - **429 / 5xx / timeout:** Geçici hata. Durum değişmez, backoff uygulanır.
3. **WHOIS (RDAP yoksa):**
   - `whois.iana.org`'a TLD sor → `refer:` satırından TLD WHOIS sunucusunu al (cache'le).
   - "No match", "NOT FOUND", "No Data Found", "is available" gibi ifadeler boşta sayılır. Bu ifadeler config'de liste olarak tutulsun, çünkü registry'ye göre değişiyor.
   - Durum ve bitiş tarihi satırları regex ile parse edilir, alan adları registry'ye göre değişebilir.
   - Not: `.tr` gibi uzantılar için WHOIS sunucusunu ve formatını hardcode etme. Referral ve config üzerinden çöz, gerçek bir çıktıyla test et.
4. **Satılık / park tespiti** (sadece kayıtlı domainlerde, ayrıca HTTP isteğinde kısa timeout):
   - NS eşleşmesi (aşağıdaki listede).
   - HTTP(S) GET (en fazla 5 redirect, max ~200 KB body):
     - Son URL bilinen bir market alan adındaysa → satılık.
     - Body'de satılık anahtar kelimeleri varsa → satılık.

## 2. RDAP durum kodu → ana durum eşlemesi

RDAP durumları küçük harf ve boşluklu gelir (RFC 8056). Öncelik sırası yukarıdan aşağıya, ilk eşleşen kazanır:

| RDAP status içeriyorsa | Ana durum |
|---|---|
| `pending delete` | `PendingDelete` |
| `redemption period` | `Redemption` |
| `client hold` veya `server hold` | `Askida` |
| `auto renew period` **ve** bitiş tarihi geçmiş | `SuresiDoldu` |
| bitiş tarihi geçmiş (status ne olursa olsun) | `SuresiDoldu` |
| bitişe ≤ eşik gün | `BitisYakin` (ayrıca park ise not düşülür) |
| park/satılık tespiti pozitif | `ParkSatilik` |
| diğer | `Aktif` |

> Not: `.com`/`.net`'te auto-renew grace süresi registrar'a göre 0–45 gün değişir. Redemption yaklaşık 30 gün, pendingDelete yaklaşık 5 gündür. Diğer uzantılarda bu süreler farklı. O yüzden **tarih tahmini değil, status kodları esas alınır.** Sadece pendingDelete düşme tahmini tarihe dayanır.

## 3. "Boşta" çift doğrulama

1. İlk kez RDAP 404 (veya WHOIS "bulunamadı") gelirse: `pending_free_confirm = 1`, `next_check_at = şimdi + 2 dk`. **Bildirim atma.**
2. İkinci kontrol de aynıysa ve NS de yoksa → `Bosta` olarak işaretle, yüksek öncelikli bildirim gönder.
3. İkinci kontrol farklıysa → bayrağı sıfırla, normal akışa dön.

## 4. Park / market NS listesi (başlangıç listesi, config'den düzenlenebilir)

Eşleşme NS'nin **sonu** ile yapılır (`ns1.sedoparking.com` → `sedoparking.com`).

```
sedoparking.com
dan.com
afternic.com
bodis.com
parkingcrew.net
above.com
parklogic.com
domainmarket.com
```

> Bu liste başlangıç içindir. Gerçek domainlerle doğrula ve gerektikçe güncelle. `domaincontrol.com` (GoDaddy varsayılan NS'i) gibi genel NS'ler **park sayılmamalı**, çünkü normal siteler de onları kullanıyor.

## 5. Market redirect alan adları

```
dan.com
sedo.com
afternic.com
atom.com
hugedomains.com
godaddy.com   # sadece URL yolu "forsale" / "domainsearch" içeriyorsa
```

## 6. Satılık anahtar kelimeleri (büyük/küçük harf duyarsız)

```
this domain is for sale
domain is for sale
buy this domain
make an offer
bu alan adı satılıktır
bu domain satılık
alan adı satılık
```

Tek kelime ("sale" vb.) ile eşleşme yapma, yanlış pozitif çok olur. Sadece kalıp ifadeler kullanılır.

## 7. Bitiş uyarıları

`uyari_esikleri = [30, 7, 1]` → her eşik, aynı bitiş tarihi için **bir kez** bildirilir (`notified_thresholds`). Domain yenilenip `expires_at` ileri giderse liste sıfırlanır ve "yenilendi" bildirimi atılır.
