# BatchMail

Dashboard dan API untuk kirim email massal (bulk/batch) lewat mail server **[Stalwart](https://stalw.art)** milik Anda sendiri. Ditulis dengan Rust.

Tujuannya sederhana: siapa pun yang punya sistem (HR, toko online, klinik, sekolah, …) cukup membuka BatchMail, membuat **API key**, lalu langsung bisa mengirim batch email lewat satu endpoint. Untuk yang tidak memakai API, batch juga bisa dikirim manual dari dashboard: upload CSV atau tempel daftar email.

- **Kirim batch manual**: upload CSV atau tempel daftar. Kolom CSV otomatis jadi variabel (`{{nama}}`, `{{kode_promo}}`, …). Ada email tes, pengiriman terjadwal, dan batas kecepatan per menit.
- **API untuk sistem lain**: satu API key per sistem, mode *live* / *test*, batas harian, dan pembatasan domain pengirim.
- **Pengiriman terkontrol**: worker di latar belakang mengirim ke Stalwart sesuai kecepatan tiap batch. Penolakan sementara (4xx) dicoba ulang otomatis. Respons SMTP tiap penerima tercatat; batch bisa dijeda, dilanjutkan, dibatalkan, atau dikirim ulang untuk yang gagal. Laporan bisa diunduh sebagai CSV.
- **Unsubscribe sesuai aturan Gmail/Yahoo**: link bertanda tangan, header `List-Unsubscribe` + one-click (RFC 8058), dan daftar suppression. Email transaksional tidak terkena.
- **Integrasi domain yang mudah**: domain dan kunci DKIM dibuat otomatis di Stalwart. BatchMail menampilkan record DNS yang perlu dipasang, lalu memverifikasi SPF/DKIM/DMARC. Record bisa dipasang **manual** (salin, atau impor file zona) atau **otomatis lewat Cloudflare**.

---

## Daftar isi

1. [Cara kerja](#cara-kerja)
2. [Prasyarat](#prasyarat)
3. [Instalasi A — Docker Compose (Stalwart + BatchMail)](#instalasi-a--docker-compose-stalwart--batchmail)
4. [Instalasi B — Dari source (tanpa Docker)](#instalasi-b--dari-source-tanpa-docker)
5. [Menyiapkan Stalwart](#menyiapkan-stalwart)
6. [Reverse proxy HTTPS](#reverse-proxy-https)
7. [Langkah pertama di dashboard](#langkah-pertama-di-dashboard)
8. [Domain & DNS: manual atau Cloudflare](#domain--dns-manual-atau-cloudflare)
9. [Referensi konfigurasi (`.env`)](#referensi-konfigurasi-env)
10. [API](#api)
11. [Variabel template & unsubscribe](#variabel-template--unsubscribe)
12. [Status batch & penerima](#status-batch--penerima)
13. [Tips deliverability](#tips-deliverability)
14. [Backup, update, dan troubleshooting](#backup-update-dan-troubleshooting)
15. [Pengembangan](#pengembangan)
16. [Batasan saat ini](#batasan-saat-ini)
17. [Lisensi](#lisensi)

---

## Cara kerja

```
 Sistem Anda ──(HTTPS + API key)──┐
                                  ▼
 Dashboard (CSV / tempel) ──► BatchMail ──(SMTP 587, AUTH)──► Stalwart ──► server penerima
                               │   ▲                            │
                               │   └──(Management API)──────────┘  domain, DKIM, record DNS, antrean
                               └──(Cloudflare API, opsional)──► DNS domain Anda
```

- BatchMail **tidak** mengirim email langsung ke internet. Pesan diserahkan ke Stalwart lewat port submission (587/465). Stalwart yang menandatangani DKIM, mengantre, dan mengantarkan ke server penerima, termasuk retry-nya.
- Status **"Diterima Stalwart"** berarti Stalwart sudah menerima pesan untuk diantar.
- Database memakai **SQLite** (satu file), jadi tidak perlu server database terpisah.

## Prasyarat

- Server Linux dengan IP publik yang **boleh mengirim lewat port 25**. Banyak VPS memblokirnya secara default; minta dibuka ke penyedia VPS.
- Hostname untuk mail server, mis. `mail.domainanda.id`, dengan record **A** ke IP server dan **PTR / reverse DNS** ke hostname yang sama (atur di panel VPS).
- Domain pengirim yang DNS-nya bisa Anda ubah.
- Untuk instalasi A: Docker + Docker Compose. Untuk instalasi B: Rust 1.85+ (`rustup`).

## Instalasi A — Docker Compose (Stalwart + BatchMail)

```bash
git clone https://github.com/haloabdur/bulkemail.git batchmail
cd batchmail
cp .env.example .env
openssl rand -hex 32        # salin hasilnya ke APP_SECRET di .env
nano .env                   # isi PUBLIC_BASE_URL, ADMIN_*, SMTP_*, MAIL_HOSTNAME
docker compose up -d stalwart
docker compose logs stalwart | grep -i password   # password admin awal Stalwart
```

1. Buka admin Stalwart di `http://IP-SERVER:8081`, login dengan `admin` + password dari log, lalu ikuti [Menyiapkan Stalwart](#menyiapkan-stalwart). Isi `SMTP_PASS` dan `STALWART_API_TOKEN` di `.env`.
2. Jalankan BatchMail:
   ```bash
   docker compose up -d --build batchmail
   docker compose logs -f batchmail      # harus muncul "terhubung ke Stalwart SMTP"
   ```
3. Pasang [reverse proxy HTTPS](#reverse-proxy-https) ke `127.0.0.1:8080`, lalu buka `PUBLIC_BASE_URL` dan login dengan `ADMIN_EMAIL` / `ADMIN_PASSWORD`.

> Sudah punya Stalwart di server lain? Jalankan BatchMail saja (`docker compose up -d --build batchmail`), dengan `SMTP_HOST` dan `STALWART_API_URL` mengarah ke server Stalwart tersebut. Bagian `stalwart` di `docker-compose.yml` boleh dihapus.

## Instalasi B — Dari source (tanpa Docker)

```bash
# 1. Build
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh   # jika belum ada Rust
git clone https://github.com/haloabdur/bulkemail.git batchmail
cd batchmail
cargo build --release
sudo install -m 755 target/release/batchmail /usr/local/bin/batchmail

# 2. User & folder data
sudo useradd --system --home /opt/batchmail --create-home batchmail
sudo cp .env.example /opt/batchmail/.env
sudo nano /opt/batchmail/.env       # isi semua nilai; DATABASE_URL=sqlite:///opt/batchmail/batchmail.db
sudo chown -R batchmail:batchmail /opt/batchmail && sudo chmod 600 /opt/batchmail/.env

# 3. Service systemd
sudo cp deploy/batchmail.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now batchmail
journalctl -u batchmail -f
```

Template HTML, CSS, dan migrasi database sudah tertanam di binary. Yang perlu disalin ke server hanya `batchmail` dan file `.env`. Migrasi database berjalan otomatis setiap start.

Perintah tambahan:

```bash
batchmail create-user email@domain.id 'password-minimal-10-karakter'   # tambah user / reset password
```

(Jalankan dengan `.env` yang sama, mis. dari `/opt/batchmail`, atau `docker compose exec batchmail batchmail create-user …`.)

## Menyiapkan Stalwart

Langkah-langkah di admin Stalwart (nama menu bisa sedikit berbeda antar versi):

1. **Hostname & TLS**: atur hostname server ke `mail.domainanda.id` dan aktifkan sertifikat TLS (ACME / Let's Encrypt), supaya STARTTLS di port 587 valid untuk hostname itu.
2. **Akun SMTP untuk BatchMail**: buat akun, mis. `batchmail@domainanda.id`, lalu isi `SMTP_USER` dan `SMTP_PASS`.
3. **Izinkan akun itu mengirim atas nama alamat lain.** Secara default Stalwart mewajibkan alamat *From* sama dengan akun yang login. Untuk akun BatchMail, nonaktifkan aturan ini atau buat pengecualian khusus akun tersebut. Pengaturannya di bagian SMTP › sesi AUTH / MAIL FROM ("must match sender"). Tanpa langkah ini, pengiriman ditolak dengan error 5xx.
4. **API key untuk Management API**: buat API key (atau pakai `admin:password`) dengan izin mengelola domain, DKIM, melihat record DNS, dan membaca antrean. Isi `STALWART_API_TOKEN`. `STALWART_API_URL` = `http://stalwart:8080/api` di Docker Compose, atau `https://mail.domainanda.id/api`.
5. **Pastikan pesan keluar ditandatangani DKIM.** Setelah menambah domain dari BatchMail, kirim email tes dan cek header `DKIM-Signature` di email yang diterima. Jika belum ada, aktifkan penandatanganan untuk domain tersebut di pengaturan DKIM Stalwart.

Endpoint Management API yang dipakai BatchMail (`src/stalwart.rs`): `POST /api/principal`, `POST /api/dkim`, `GET /api/dns/records/{domain}`, `GET /api/queue/messages`. Jika versi Stalwart Anda berbeda, BatchMail tetap jalan: record SPF/DMARC dibuat sendiri, dan DKIM bisa diatur manual (isi selector saat menambah domain).

## Reverse proxy HTTPS

BatchMail mendengarkan HTTP di `BIND_ADDR` (default `0.0.0.0:8080`). Di produksi, pasang di belakang HTTPS. Halaman unsubscribe juga dibuka penerima dari internet. Contoh **Caddy** (sertifikat otomatis):

```caddyfile
batchmail.domainanda.id {
    reverse_proxy 127.0.0.1:8080
}
```

Contoh **Nginx**:

```nginx
server {
    listen 443 ssl http2;
    server_name batchmail.domainanda.id;
    # ssl_certificate ... (certbot)
    client_max_body_size 32m;          # upload CSV besar
    location / {
        proxy_pass http://127.0.0.1:8080;
        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Proto https;
    }
}
```

Jika `PUBLIC_BASE_URL` diawali `https://`, cookie login otomatis diberi flag `Secure`.

## Langkah pertama di dashboard

1. **Login** dengan `ADMIN_EMAIL` / `ADMIN_PASSWORD`. Akun ini hanya dibuat saat database masih kosong.
2. **Koneksi Stalwart**: klik *Tes koneksi*. Status Management API harus "Terhubung".
3. **Domain & DNS** › *Tambah domain*, misalnya `domainanda.id`. Pasang record DNS-nya (lihat bagian berikut) sampai status **Siap dipakai**.
4. **Kirim batch** › upload CSV atau tempel daftar › *Kirim email tes ke saya* › *Kirim batch*.
5. **API keys** › buat satu key per sistem. Simpan key-nya, karena hanya ditampilkan sekali. Contoh pemanggilan ada di halaman itu dan di bagian [API](#api).

## Domain & DNS: manual atau Cloudflare

Saat domain ditambahkan, BatchMail (lewat Management API) membuat domain dan kunci DKIM di Stalwart, lalu menyusun record yang dibutuhkan:

| Fungsi | Tipe | Nama | Nilai | Wajib |
|---|---|---|---|---|
| SPF | TXT | `domainanda.id` | `v=spf1 a:mail.domainanda.id ~all` | Ya |
| DKIM | TXT | `<selector>._domainkey.domainanda.id` | kunci publik dari Stalwart | Ya |
| DMARC | TXT | `_dmarc.domainanda.id` | `v=DMARC1; p=none; rua=mailto:postmaster@domainanda.id` | Sangat disarankan |
| Menerima email | MX, dll. | — | dari Stalwart | Hanya bila email **masuk** juga ke Stalwart |

SPF memakai `a:<MAIL_HOSTNAME>` (atau `ip4:`/`ip6:` bila berupa IP). Domain bisa dipakai mengirim setelah **SPF dan DKIM lolos**. DMARC sangat disarankan; Gmail & Yahoo mewajibkannya untuk pengirim massal.

> **Jangan pasang MX** dari Stalwart bila kotak masuk domain Anda ada di layanan lain (Google Workspace, Zoho, cPanel, …). Untuk mengirim saja, MX tidak diperlukan.

### Manual

Di halaman domain, tab **Manual**:

- Salin nilai tiap record (tombol *Salin nilai*), lalu tambahkan di panel DNS penyedia Anda. Banyak panel menulis nama tanpa domain, mis. `_dmarc` atau `stalwart._domainkey`.
- Atau salin **file zona (format BIND)** dan impor di penyedia yang mendukung (mis. Cloudflare › DNS › Records › *Import and Export*).
- Sudah punya SPF? **Jangan buat record SPF kedua.** Tambahkan `a:mail.domainanda.id` ke record yang ada, sebelum `~all`.
- Klik **Periksa ulang DNS** setelah beberapa menit.

### Otomatis lewat Cloudflare

1. Di Cloudflare: **My Profile › API Tokens › Create Token** › template **Edit zone DNS**. Batasi ke zona domain Anda (*Zone Resources › Include › Specific zone*).
2. Di BatchMail, buka domain › tab **Otomatis · Cloudflare** › tempel token › *Hubungkan Cloudflare*. Token disimpan **terenkripsi** (ChaCha20-Poly1305, kunci dari `APP_SECRET`). Alternatif: isi `CLOUDFLARE_API_TOKEN` di `.env` untuk dipakai semua domain.
3. Klik **Pratinjau perubahan** untuk melihat apa yang akan dibuat atau diubah, lalu **Pasang record di Cloudflare**. BatchMail langsung memeriksa ulang DNS.

Aturan pengamannya (diuji di `src/cloudflare.rs`):

- **SPF** yang sudah ada **tidak diganti**; BatchMail hanya menyisipkan `a:mail.domainanda.id` sebelum `~all`/`-all`. Record SPF kedua tidak pernah dibuat.
- **DMARC** yang sudah ada tidak diubah.
- **DKIM** dibuat atau diperbarui sesuai kunci dari Stalwart.
- Record penerimaan (MX dll.) hanya dipasang bila dicentang, dan **tidak pernah menimpa** MX/CNAME yang sudah ada.
- Subdomain (mis. `klinik.domainanda.id`) otomatis memakai zona induknya.

## Referensi konfigurasi (`.env`)

| Variabel | Wajib | Default | Keterangan |
|---|---|---|---|
| `APP_SECRET` | ya | — | ≥32 karakter acak. Menandatangani link unsubscribe & mengenkripsi token Cloudflare. **Jangan diganti** setelah dipakai. |
| `PUBLIC_BASE_URL` | ya | `http://localhost:8080` | Alamat publik BatchMail (untuk link unsubscribe). |
| `SMTP_HOST` | ya | — | Host Stalwart (sesuai nama di sertifikat TLS). |
| `SMTP_PORT` | | 587 / 465 / 25 | Mengikuti `SMTP_TLS`. |
| `SMTP_TLS` | | `starttls` | `starttls`, `tls`, atau `none` (hanya jaringan privat). |
| `SMTP_USER`, `SMTP_PASS` | | — | Akun SMTP Stalwart untuk BatchMail. |
| `MAIL_HOSTNAME` | | = `SMTP_HOST` | Hostname publik Stalwart, dipakai di SPF. Isi bila `SMTP_HOST` berupa nama internal/IP. |
| `STALWART_API_URL` | | — | Mis. `https://mail.domainanda.id/api`. |
| `STALWART_API_TOKEN` | | — | API key Stalwart (Bearer) atau `admin:password`. |
| `CLOUDFLARE_API_TOKEN` | | — | Token default untuk DNS otomatis. |
| `DATABASE_URL` | | `sqlite://data/batchmail.db` | Lokasi file SQLite (folder dibuat otomatis). |
| `BIND_ADDR` | | `0.0.0.0:8080` | Alamat HTTP. |
| `ADMIN_EMAIL`, `ADMIN_PASSWORD` | | — | User pertama (dibuat bila belum ada user). Password ≥10 karakter. |
| `DEFAULT_RATE_PER_MINUTE` | | 400 | Kecepatan default per batch (1–10.000). |
| `MAX_RECIPIENTS_API` | | 10000 | Maks. penerima per request API. |
| `MAX_RECIPIENTS_DASHBOARD` | | 100000 | Maks. penerima per batch dari dashboard. |
| `TZ_OFFSET_HOURS` | | 7 | Zona waktu tampilan & jadwal (WIB 7, WITA 8, WIT 9). |
| `ALLOW_UNVERIFIED_DOMAINS` | | `false` | Hanya untuk development. |
| `RUST_LOG` | | `batchmail=info` | Level log. |

## API

Base URL: `https://batchmail.domainanda.id/api/v1`. Autentikasi: header `Authorization: Bearer bk_live_…`.

| Method | Path | Fungsi |
|---|---|---|
| `POST` | `/batches` | Kirim batch (maks. `MAX_RECIPIENTS_API` penerima per request) |
| `POST` | `/emails` | Kirim satu email (default transaksional) |
| `GET` | `/batches/{id}` | Status & hitungan batch |
| `GET` | `/batches/{id}/recipients?status=&limit=&offset=` | Status per penerima + respons SMTP |
| `DELETE` | `/batches/{id}` | Batalkan batch yang belum selesai |
| `GET` | `/domains` | Domain yang boleh dipakai key ini |

Sebuah key hanya bisa melihat batch yang ia buat sendiri.

### Kirim batch

```bash
curl -X POST https://batchmail.domainanda.id/api/v1/batches \
  -H "Authorization: Bearer $BATCHMAIL_KEY" \
  -H "Content-Type: application/json" \
  -d '{
    "name": "Pengingat jadwal 26 Okt",
    "from": "Klinik Sehat <klinik@domainanda.id>",
    "reply_to": "cs@domainanda.id",
    "subject": "Pengingat jadwal, {{nama}}",
    "html": "<p>Halo {{nama}}, jadwal Anda pukul {{jam}}.</p>",
    "text": "Halo {{nama}}, jadwal Anda pukul {{jam}}.",
    "kind": "marketing",
    "send_at": "2026-10-26T07:00:00+07:00",
    "rate_per_minute": 300,
    "recipients": [
      { "email": "andi@contoh.co.id", "vars": { "nama": "Andi", "jam": "09:00" } },
      { "email": "siti@contoh.com",   "vars": { "nama": "Siti", "jam": "10:30" } }
    ]
  }'
```

| Field | Wajib | Keterangan |
|---|---|---|
| `from` | ya | `email` atau `Nama <email>`. Domainnya harus terdaftar & terverifikasi. |
| `subject` | ya | Boleh berisi variabel. |
| `html` / `text` | salah satu | Kirim keduanya untuk hasil terbaik. |
| `recipients` | ya | `[{ "email", "vars": { … } }]`. Invalid/duplikat/unsubscribe dilewati dan dilaporkan. |
| `kind` | | `marketing` (default) atau `transactional`. |
| `send_at` | | RFC 3339. Kosong = sekarang. |
| `rate_per_minute` | | 1–10.000. |
| `name`, `reply_to` | | Opsional. |

Respons `202 Accepted`:

```json
{
  "id": "b_2Ky4hTq9Lm",
  "status": "scheduled",
  "accepted": 2,
  "rejected": [],
  "status_url": "/api/v1/batches/b_2Ky4hTq9Lm"
}
```

`rejected` berisi `{ "email", "reason" }`, dengan `reason` = `invalid_email`, `duplicate`, atau `unsubscribed`.

### Kirim satu email

```bash
curl -X POST https://batchmail.domainanda.id/api/v1/emails \
  -H "Authorization: Bearer $BATCHMAIL_KEY" -H "Content-Type: application/json" \
  -d '{ "from": "noreply@domainanda.id", "to": "andi@contoh.co.id",
        "subject": "Reset password", "html": "<p>Kode Anda: {{kode}}</p>", "vars": { "kode": "482910" } }'
```

### Cek status

```bash
curl -H "Authorization: Bearer $BATCHMAIL_KEY" https://batchmail.domainanda.id/api/v1/batches/b_2Ky4hTq9Lm
```

```json
{
  "id": "b_2Ky4hTq9Lm", "name": "Pengingat jadwal 26 Okt", "status": "sending",
  "kind": "marketing", "test_mode": false, "total": 2,
  "counts": { "queued": 1, "sent": 1, "deferred": 0, "failed": 0, "skipped": 0, "cancelled": 0 },
  "send_at": "2026-10-26T00:00:00+00:00", "created_at": "…", "finished_at": null
}
```

### Error

Semua error berbentuk `{ "error": { "code": "…", "message": "…" } }`.

| HTTP | `code` | Arti |
|---|---|---|
| 400 | `invalid_json` | Body bukan JSON yang valid |
| 401 | `unauthorized` | Key salah atau sudah dicabut |
| 403 | `domain_not_allowed` | Key dibatasi ke domain lain |
| 404 | `not_found` | Batch tidak ada / bukan milik key ini |
| 409 | `not_cancellable` | Batch sudah selesai |
| 422 | `unknown_domain`, `domain_not_verified`, `invalid_from`, `missing_subject`, `missing_body`, `too_many_recipients`, `no_valid_recipients`, `invalid_send_at`, … | Validasi |
| 429 | `daily_limit` | Batas harian key terlampaui |

### Mode test

Key `bk_test_…` (pilih *Mode: Test* saat membuat key) memproses batch seperti biasa, tetapi **tidak mengirim apa pun**. Semua penerima ditandai terkirim dengan respons `250 Mode test`. Cocok untuk staging.

## Variabel template & unsubscribe

- `{{nama_kolom}}` diganti dengan nilai dari kolom CSV (nama kolom diubah ke huruf kecil, spasi jadi `_`) atau dari `vars` di API. Variabel yang tidak ada diganti kosong.
- Di HTML, nilai variabel di-*escape* otomatis.
- Variabel bawaan: `{{email}}` dan `{{unsubscribe_url}}`.
- Email **marketing** selalu mendapat header `List-Unsubscribe` + `List-Unsubscribe-Post` (one-click). Jika isi email tidak memuat `{{unsubscribe_url}}`, BatchMail menambahkan link "Berhenti berlangganan" di bagian bawah.
- Penerima yang berhenti berlangganan masuk **daftar Unsubscribe** (menu *Unsubscribe*). Mereka dilewati di batch marketing berikutnya. Email **transactional** tetap terkirim.

## Status batch & penerima

| Batch | Arti |
|---|---|
| `scheduled` | Menunggu `send_at` |
| `queued` / `sending` | Sedang dikirim |
| `paused` | Dijeda dari dashboard |
| `completed` | Semua penerima sudah diproses |
| `cancelled` | Dibatalkan |

| Penerima | Arti |
|---|---|
| `queued` | Menunggu giliran |
| `sent` | Diterima Stalwart untuk diantar |
| `deferred` | Ditolak sementara (4xx); dicoba ulang setelah 1 menit, lalu 5 menit (maks. 3 percobaan) |
| `failed` | Ditolak permanen (5xx) atau gagal 3× |
| `skipped` | Penerima berhenti berlangganan setelah batch dibuat |
| `cancelled` | Batch dibatalkan sebelum terkirim |

Jika Stalwart tidak bisa dihubungi, pengiriman otomatis berhenti sebentar. Penerima tetap di antrean dan tidak dihitung gagal.

## Tips deliverability

- Pasang **PTR/reverse DNS** IP server ke `mail.domainanda.id`, dan pastikan hostname itu punya record A ke IP yang sama.
- Pastikan **SPF, DKIM, dan DMARC** lolos (halaman Domain & DNS). Setelah stabil, naikkan DMARC dari `p=none` ke `p=quarantine`.
- **Pemanasan IP baru**: mulai dari ratusan email per hari, naikkan bertahap, dan pakai `rate_per_minute` yang rendah di awal.
- Hanya kirim ke orang yang setuju menerima. Bersihkan alamat yang gagal permanen dari daftar Anda.
- Pakai domain/subdomain terpisah untuk marketing (mis. `news.domainanda.id`) agar reputasi email transaksional terjaga.

## Backup, update, dan troubleshooting

**Backup**: cukup file SQLite + `.env` (terutama `APP_SECRET`).

```bash
sqlite3 /opt/batchmail/batchmail.db ".backup '/backup/batchmail-$(date +%F).db'"
# Docker:
docker compose exec batchmail sh -c 'cat /data/batchmail.db' > batchmail-$(date +%F).db
```

**Update**: `git pull && cargo build --release` (atau `docker compose up -d --build batchmail`). Migrasi berjalan otomatis.

**Troubleshooting**:

| Gejala | Penyebab umum |
|---|---|
| Log: `belum bisa terhubung ke Stalwart SMTP` | Host/port salah, firewall, atau sertifikat TLS tidak cocok dengan `SMTP_HOST`. |
| Penerima `failed` dengan `5xx … not allowed to send as` | Aturan "must match sender" di Stalwart belum dinonaktifkan untuk akun BatchMail. |
| Penerima `failed` dengan `535 authentication` | `SMTP_USER`/`SMTP_PASS` salah. |
| Domain tidak bisa dipakai: `domain_not_verified` | SPF/DKIM belum terpasang atau belum terlihat. Klik *Periksa ulang DNS* beberapa menit kemudian. |
| SPF tidak pernah "Terpasang" | Record SPF tidak memuat `a:<MAIL_HOSTNAME>`, atau ada dua record SPF. |
| Management API "Gagal" | URL salah (harus diakhiri `/api`) atau token tidak punya izin. |
| Cloudflare: "Zona … tidak ditemukan" | Token tidak punya akses ke zona tersebut, atau domain tidak memakai DNS Cloudflare. |
| Email masuk spam | Lihat [Tips deliverability](#tips-deliverability); cek header `DKIM-Signature` dan hasil `Authentication-Results`. |

## Pengembangan

```bash
cp .env.example .env    # isi APP_SECRET dan SMTP_HOST minimal
cargo run               # http://localhost:8080
cargo test              # unit + integrasi (database in-memory, tanpa SMTP)
cargo fmt && cargo clippy
```

Untuk mencoba tanpa Stalwart, pakai server SMTP dummy (mis. [Mailpit](https://mailpit.axllent.org): `SMTP_HOST=localhost SMTP_PORT=1025 SMTP_TLS=none`) dengan `ALLOW_UNVERIFIED_DOMAINS=true`.

Struktur kode:

| File | Isi |
|---|---|
| `src/api.rs` | REST API `/api/v1` |
| `src/web.rs`, `src/web_domains.rs`, `templates/` | Dashboard (server-rendered, Askama) |
| `src/batches.rs` | Validasi & pembuatan batch, parsing CSV/daftar |
| `src/worker.rs` | Pengirim latar belakang (rate limit, retry) |
| `src/mailer.rs` | SMTP ke Stalwart (lettre) |
| `src/stalwart.rs` | Klien Management API Stalwart |
| `src/domains.rs`, `src/dns.rs`, `src/cloudflare.rs` | Domain, verifikasi DNS, integrasi Cloudflare |
| `src/unsubscribe.rs` | Link unsubscribe bertanda tangan |
| `migrations/` | Skema SQLite |

## Batasan saat ini

- **Bounce asinkron belum dibaca.** Penolakan yang terjadi *setelah* Stalwart menerima pesan (server penerima menolak belakangan) belum masuk ke log BatchMail. Lihat di antrean/report Stalwart. Rencana berikutnya: menerima webhook/DSN dari Stalwart.
- Belum ada webhook status ke sistem Anda. Pakai `GET /batches/{id}` untuk polling.
- Belum ada "list tersimpan" (kontak permanen). Setiap batch membawa daftarnya sendiri.
- Satu workspace: semua user dashboard melihat semua batch, key, dan domain.
- Pengaturan SMTP dibaca dari `.env` (halaman *Koneksi Stalwart* hanya menampilkan dan menguji).

## Lisensi

[MIT](LICENSE)
