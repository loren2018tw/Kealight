# Simple Password Authentication

Kealight 加入全站密碼認證。採用 Cookie-based login page，單一共享密碼（無帳號欄位），明文比較，session cookie（HttpOnly + SameSite=Lax，不含 Secure）。

密碼設定於 `kealight.toml` 的 `password` 欄位，空字串或缺省時不啟用認證。未認證時所有路由（含唯讀）皆需通過，302 導向 `/login`。實作使用 axum middleware（tower layer）攔截所有請求。
