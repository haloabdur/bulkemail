# Mencoba BatchMail dengan Postman (dan siap dipakai)

Panduan ini membawa Anda dari nol sampai sistem Anda bisa kirim batch email dan tahu mana yang bounce.
Koleksi siap impor: [`batchmail.postman_collection.json`](batchmail.postman_collection.json).

- **Bagian A**: yang harus disiapkan.
- **Bagian B**: uji lengkap di Postman memakai key *test* (tidak mengirim email sungguhan).
- **Bagian C**: sambungkan bounce dari Stalwart.
- **Bagian D**: cara memakainya di sistem Anda.

---

## A. Yang harus disiapkan

### Untuk uji coba di Postman (sekitar 15 menit)

| # | Siapkan | Cara |
|---|---|---|
| 1 | BatchMail berjalan | Docker Compose atau binary, lihat README › Instalasi |
| 2 | `BOUNCE_WEBHOOK_TOKEN` di `.env` | `openssl rand -hex 24`, lalu restart BatchMail. Tanpa ini endpoint bounce mati (404). |
| 3 | Satu domain terdaftar | Dashboard › *Domain & DNS* › *Tambah domain*. Untuk uji test-key, SPF/DKIM boleh belum lolos bila `ALLOW_UNVERIFIED_DOMAINS=true` (hanya di komputer uji). |
| 4 | API key **mode Test** | Dashboard › *API keys* › nama bebas, *Mode: Test*. Key ditampilkan **sekali**, salin (`bk_test_…`). |
| 5 | Postman | Desktop atau web. |

Key test memproses batch seperti biasa, tetapi **tidak mengirim apa pun**. Alamat contoh di koleksi (`contoh.co.id`) aman dipakai.

### Untuk pemakaian sungguhan (sebelum key *live*)

Urutannya penting. Lewati satu, dan deliverability Anda yang rugi.

1. **Server**: IP publik yang boleh kirim lewat port 25, **PTR/reverse DNS** ke `mail.domainanda.id`, dan record A hostname itu ke IP yang sama.
2. **Stalwart** jalan: TLS valid, akun SMTP untuk BatchMail, aturan *must match sender* dimatikan untuk akun itu (README › Menyiapkan Stalwart).
3. **Domain pengirim**: SPF, DKIM, dan DMARC semuanya hijau di *Domain & DNS*. Pakai **subdomain** khusus marketing, mis. `news.domainanda.id`.
4. **HTTPS** untuk BatchMail (reverse proxy). Link unsubscribe dibuka penerima dari internet, dan Stalwart perlu menjangkau `/hooks/bounce`.
5. **Pemanasan IP baru**: mulai ratusan email per hari, `rate_per_minute` rendah, naikkan bertahap.
6. **Daftar email Anda**: hanya alamat yang memang setuju menerima. Satu kolom `email`, kolom lain jadi variabel.
7. **Bounce terhubung** (Bagian C). Tanpa ini, alamat mati tetap dikirimi terus dan reputasi IP turun.
8. **Backup**: file SQLite dan `.env` (terutama `APP_SECRET`).

---

## B. Uji lengkap di Postman

### B1. Impor dan isi variabel
1. Postman › **Import** › pilih `docs/batchmail.postman_collection.json`.
2. Buka koleksi **BatchMail** › tab **Variables**, isi kolom *Current value*:

| Variabel | Isi |
|---|---|
| `baseUrl` | alamat BatchMail, mis. `http://localhost:8080` atau `https://batchmail.domainanda.id` |
| `apiKey` | key test dari A.4 |
| `bounceToken` | nilai `BOUNCE_WEBHOOK_TOKEN` |
| `fromAddress` | alamat pengirim di domain terdaftar, mis. `info@domainanda.id` |

Variabel lain (`emailOk`, `emailBounce`, `emailSpam`, `batchId`, `messageId…`) sudah terisi atau diisi otomatis. Biarkan.

> Di badan request ada `{{nama}}` dan `{{kode}}`. Itu variabel template BatchMail. Postman membiarkannya karena tidak ada variabel Postman dengan nama itu, jadi BatchMail yang mengisinya.

### B2. Jalankan berurutan

Klik koleksi › **Run**, atau jalankan request satu per satu dari atas ke bawah. Beri jeda 2–5 detik setelah *Buat batch*. Semua request punya pengecekan otomatis (tab *Test Results*), dan semuanya harus hijau.

**Folder 1 · Cek koneksi**
1. **Domain yang boleh dipakai key ini** → `200` dan domain Anda muncul. `401` berarti key salah.

**Folder 2 · Kirim batch**

2. **Buat batch (3 penerima)** → `202`, `accepted: 3`. `batchId` tersimpan otomatis.
   `422 domain_not_verified` berarti domain belum lolos. Lihat A.3.
3. **Status batch** → `counts.sent: 3`. Jika masih `queued`, tunggu sebentar lalu kirim ulang request ini.
4. **Daftar penerima + simpan message_id** → tiap penerima punya `message_id` seperti `<b_AbCdEfGhIj.2.xxxxxxxx@domainanda.id>`. Skrip koleksi menyimpannya untuk folder 3. ID inilah yang dipakai BatchMail untuk mengenali email mana yang bounce.
5. **Kirim satu email (transaksional)** → `202`.

**Folder 3 · Simulasi bounce** (memakai `bounceToken`, bukan `apiKey`)

6. **Hard bounce: user unknown** → `{"received":1,"results":{"bounced":1}}`.
7. **Soft bounce** → `results.soft: 1`. Hanya dicatat, status penerima tidak berubah.
8. **Komplain spam** → `results.complained: 1`.
9. **Email DSN mentah** → `results.duplicate: 1`. Isinya sama dengan langkah 6 dalam format email DSN, jadi ini juga membuktikan webhook yang diulang tidak dihitung dua kali.
10. **Log event bounce terbaru** → melihat apa yang diterima dan diputuskan.
11. **Tanpa token** → `401`.

**Folder 4 · Hasil setelah bounce**

12. **Status batch** → `counts`: `sent: 1, bounced: 1, complained: 1`.
13. **Hanya yang bounce** → penerima `budi@…`, `smtp_response: "Bounce 5.1.1: …"`.
14. **Hanya yang komplain** → penerima `citra@…`.
15. **Kirim ulang ke alamat yang sudah bounce** → `rejected: [{ "reason": "bounced" }]`. Inilah manfaat utamanya: alamat mati tidak dikirimi lagi.
16. **Batalkan batch** → `409`. Itu normal, karena batch sudah selesai dan tidak bisa dibatalkan.

Di dashboard: halaman batch menampilkan kolom *Bounce / komplain*, dan menu *Unsubscribe* menampilkan alamat dengan sumber `bounce` dan `complaint`.

**Menjalankan ulang:** `emailBounce` dan `emailSpam` sekarang ada di daftar suppression, jadi proses kedua akan gagal di langkah 2. Ganti kedua alamat itu di variabel koleksi (mis. `budi2@…`, `citra2@…`), atau hapus keduanya dari menu *Unsubscribe*.

### B3. Aturan yang perlu Anda tahu

| Kejadian | Status penerima | Alamat disuppress? |
|---|---|---|
| Hard bounce, kode `5.1.x` (user unknown, domain tidak ada) atau teks "user unknown" tanpa kode | `bounced` | **Ya** |
| Hard bounce lain: `5.7.x` (diblokir/spam), `5.2.x` (kotak penuh), dll. | `bounced` | Tidak. Itu masalah kita atau kondisi sementara, bukan alamatnya. |
| Komplain (`feedback-type:` atau `"type":"complaint"`) | `complained` | **Ya** |
| Soft bounce (4.x.x, delayed) | tetap `sent` | Tidak, hanya dicatat |
| Event sukses (delivered, dst.) | tidak berubah | Tidak |

Alamat yang disuppress karena bounce atau komplain dilewati untuk **semua** email, termasuk transaksional. Unsubscribe biasa hanya menghentikan email marketing.

---

## C. Menyambungkan bounce dari Stalwart

> **Status verifikasi:** BatchMail mengenali bounce lewat `Message-ID` miliknya, yang ada di payload apa pun yang membawa email asli: DSN mentah atau event JSON dengan Message-ID. Karena itu tidak terkunci ke satu skema event. Namun **saya belum bisa menguji dengan Stalwart sungguhan**, dan dokumentasi daftar event Stalwart tidak bisa saya baca saat membangun ini. Jalankan langkah 3 di bawah untuk memastikan event yang dikirim Stalwart benar-benar dikenali.

1. Pastikan `BOUNCE_WEBHOOK_TOKEN` terisi dan BatchMail bisa dijangkau Stalwart lewat HTTPS.
2. Di Stalwart, tambahkan **webhook** (Settings › Telemetry › Webhooks, atau bagian `webhook.<id>` di konfigurasi; lihat [dokumentasi webhook Stalwart](https://stalw.art/docs/telemetry/webhooks)):
   - **URL**: `https://batchmail.domainanda.id/hooks/bounce?log=all`
   - **Autentikasi**: salah satu dari ini sudah diterima BatchMail.
     - HTTP Basic dengan password = token. Username bebas.
     - `signature-key` = token. BatchMail memeriksa header `X-Signature` (HMAC-SHA256, base64).
     - atau tambahkan `&token=…` pada URL.
   - **Events**: mulai dengan event pengiriman, lalu persempit ke event gagal/DSN setelah langkah 3. Daftar lengkapnya ada di halaman Event Types dokumentasi Stalwart.
3. Kirim batch kecil (3 alamat) ke satu alamat yang sengaja salah, mis. `tidak-ada-123@gmail.com`, memakai **key live** dari domain Anda. Beberapa menit kemudian buka `GET /hooks/bounce/events`:
   - outcome `bounced`: **berhasil**. Hapus `?log=all` dari URL webhook.
   - outcome `unmatched` atau `ignored`: lihat `payload`-nya. Event gagal dari Stalwart tidak membawa Message-ID asli, atau bentuknya belum dikenali. Kirim contoh `payload` itu ke saya, dan parser tinggal disesuaikan.
   - tidak ada event sama sekali: webhook belum menjangkau BatchMail. Periksa URL, TLS, token, dan firewall.
4. `?log=all` menyimpan semua event, termasuk yang bukan kegagalan. Pakai hanya saat setup. Tabel dibatasi 1000 baris terakhir.

Alternatif bila event Stalwart tidak cocok: teruskan email DSN yang Stalwart kirim ke pengirim ke endpoint yang sama. Format mentahnya diterima apa adanya (lihat request *Email DSN mentah* di koleksi).

---

## D. Memakainya di sistem Anda

Pola yang disarankan untuk daftar email yang Anda punya sendiri:

```
sistem Anda ──POST /api/v1/batches──► BatchMail ──► Stalwart ──► penerima
     ▲                                    │              │
     └── GET /batches/{id} (polling) ◄────┘              └─ bounce ─► POST /hooks/bounce
```

1. **Satu API key per sistem** (dashboard › API keys). Pakai key *live*, batasi ke satu domain, dan isi batas harian.
2. **Kirim batch**: `POST /api/v1/batches` dengan `recipients: [{ "email", "vars" }]`, maksimal 10.000 per request. Pecah daftar yang lebih besar jadi beberapa batch. Simpan `id` dari respons.
3. **Baca `rejected`** di respons: `invalid_email`, `duplicate`, `unsubscribed`, `bounced`. Hapus alamat `invalid_email` dan `bounced` dari daftar Anda.
4. **Pantau**: `GET /api/v1/batches/{id}` tiap beberapa menit sampai `status: completed`. Bounce biasanya tiba **setelah** `completed`, dari menit sampai beberapa jam, jadi cek lagi nanti.
5. **Ambil bounce** untuk membersihkan daftar Anda sendiri: `GET /api/v1/batches/{id}/recipients?status=bounced` dan `?status=complained`. Lakukan 1 hari setelah kirim, lalu sekali lagi 3 hari setelahnya.
6. **Jadwal**: `send_at` (RFC 3339) untuk kirim terjadwal, `rate_per_minute` untuk mengatur kecepatan.

Contoh minimal dari sistem Anda (bash):

```bash
curl -X POST "$BASE/api/v1/batches" \
  -H "Authorization: Bearer $BATCHMAIL_KEY" -H "Content-Type: application/json" \
  -d '{"name":"Promo November","from":"Tim <info@news.domainanda.id>",
       "subject":"Halo {{nama}}","html":"<p>Halo {{nama}}</p>","kind":"marketing",
       "rate_per_minute":300,
       "recipients":[{"email":"andi@contoh.co.id","vars":{"nama":"Andi"}}]}'
```

Aturan praktis yang menjaga reputasi:
- Bounce hard di atas **2%** dari satu batch berarti daftar Anda kotor. Berhenti dan bersihkan sebelum kirim lagi.
- `kind: "transactional"` hanya untuk email satu-satu yang dipicu tindakan pengguna (struk, reset password). Jangan dipakai untuk promosi.
- Setelah bounce terhubung dan stabil, naikkan DMARC dari `p=none` ke `p=quarantine`.

---

## E. Masalah umum

| Gejala | Penyebab |
|---|---|
| `401` di folder 1–2 | `apiKey` salah/dicabut |
| `404` di folder 3 | `BOUNCE_WEBHOOK_TOKEN` belum diisi atau BatchMail belum direstart |
| `401` di folder 3 | `bounceToken` tidak sama dengan `.env` |
| `422 domain_not_verified` | SPF/DKIM domain belum lolos |
| `422 no_valid_recipients` | Semua alamat tidak valid, duplikat, atau sudah tersuppress |
| Folder 3 hasilnya `unmatched` | `messageId…` kosong. Jalankan *Daftar penerima + simpan message_id* dulu. |
| Proses kedua gagal di *Buat batch* | Alamat uji sudah tersuppress dari proses pertama. Lihat catatan *Menjalankan ulang* di B2. |
| `results.ignored` untuk event Stalwart | Event itu bukan kegagalan (normal), atau belum dikenali. Lihat C.3. |
