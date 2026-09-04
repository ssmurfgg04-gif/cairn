/* cairn local console — poll the loopback JSON gateway, render honestly.
   No build step, no framework, no fake data: empty states stay empty until real
   data exists (taste-skill: no placeholder content).

   Round 18 additions: i18n (en/de/ja/zh — studios are international), a
   real file list with per-file sync badges, the Team card (members, my
   role, join code, audit trail), cross-project search, the storage quota
   meter, the honest update chip, the help overlay (?), and an onboarding
   hero on the zero-root state. Every server-provided string is escaped
   before it touches innerHTML. */

"use strict";

const $ = (id) => document.getElementById(id);

/* ---------- i18n (round 18, audit #10) ---------- */

const STR = {
  "brand.sub": { en: "local console", "de-DE": "Lokale Konsole", "ja-JP": "ローカルコンソール", "zh-CN": "本地控制台" },
  "nav.overview": { en: "Overview", "de-DE": "Übersicht", "ja-JP": "概要", "zh-CN": "总览" },
  "nav.projects": { en: "Projects", "de-DE": "Projekte", "ja-JP": "プロジェクト", "zh-CN": "项目" },
  "nav.files": { en: "Files", "de-DE": "Dateien", "ja-JP": "ファイル", "zh-CN": "文件" },
  "nav.activity": { en: "Activity", "de-DE": "Aktivität", "ja-JP": "アクティビティ", "zh-CN": "动态" },
  "nav.review": { en: "Review", "de-DE": "Review", "ja-JP": "レビュー", "zh-CN": "审阅" },
  "nav.team": { en: "Team", "de-DE": "Team", "ja-JP": "チーム", "zh-CN": "团队" },
  "nav.live": { en: "Live", "de-DE": "Live", "ja-JP": "ライブ", "zh-CN": "实时" },
  "nav.locks": { en: "Locks", "de-DE": "Sperren", "ja-JP": "ロック", "zh-CN": "锁定" },
  "nav.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "nav.pins": { en: "Pins", "de-DE": "Angeheftet", "ja-JP": "ピン留め", "zh-CN": "置顶" },
  "nav.recall": { en: "Recall", "de-DE": "Abrufen", "ja-JP": "リコール", "zh-CN": "取回" },
  "nav.storage": { en: "Storage", "de-DE": "Speicher", "ja-JP": "ストレージ", "zh-CN": "存储" },
  "nav.flags": { en: "Kill switches", "de-DE": "Not-Schalter", "ja-JP": "キルスイッチ", "zh-CN": "开关" },
  "nav.doctor": { en: "Doctor", "de-DE": "Arzt", "ja-JP": "ドクター", "zh-CN": "诊断" },

  "head.kicker": { en: "project folder", "de-DE": "Projektordner", "ja-JP": "プロジェクトフォルダー", "zh-CN": "项目文件夹" },
  "head.noRoots": { en: "no roots attached", "de-DE": "keine Wurzeln verbunden", "ja-JP": "接続されたルートはありません", "zh-CN": "尚未连接项目" },
  "search.placeholder": { en: "search files, projects, reviews…", "de-DE": "Dateien, Projekte, Reviews suchen…", "ja-JP": "ファイル・プロジェクト・レビューを検索…", "zh-CN": "搜索文件、项目、审阅…" },

  "ob.title.welcome": { en: "Welcome to cairn", "de-DE": "Willkommen bei cairn", "ja-JP": "cairn へようこそ", "zh-CN": "欢迎使用 cairn" },
  "ob.sub.welcome": { en: "Get your studio syncing in three steps — the tray dot turns green when you are done.", "de-DE": "In drei Schritten zum Studio-Sync — das Tray-Symbol wird grün, wenn alles läuft.", "ja-JP": "3ステップでスタジオの同期を開始 — 完了するとトレイのドットが緑になります。", "zh-CN": "三步开启工作室同步 — 完成后托盘点亮起绿灯。" },
  "ob.title.rail": { en: "Get your studio syncing", "de-DE": "Studio-Sync starten", "ja-JP": "スタジオ同期を開始", "zh-CN": "开启工作室同步" },
  "ob.step1.name": { en: "Connect a folder", "de-DE": "Ordner verbinden", "ja-JP": "フォルダーを接続", "zh-CN": "连接文件夹" },
  "ob.step1.desc": { en: "Attach the project folder your edit lives in.", "de-DE": "Den Projektordner mit deinem Schnittprojekt verbinden.", "ja-JP": "編集プロジェクトのあるフォルダーを接続します。", "zh-CN": "连接你的剪辑工程所在的文件夹。" },
  "ob.step2.name": { en: "First sync", "de-DE": "Erste Synchronisierung", "ja-JP": "初回の同期", "zh-CN": "首次同步" },
  "ob.step2.desc": { en: "Files are chunked and sent to your peers — watch the outbox drain.", "de-DE": "Dateien werden gestückelt und an Peers gesendet — der Outbox leert sich.", "ja-JP": "ファイルはチャンク化されピアへ送信 — アウトボックスの消化を見守ります。", "zh-CN": "文件分块后发送到各节点 — 观察发件箱清空。" },
  "ob.step3.name": { en: "Ready to edit", "de-DE": "Bereit zum Schneiden", "ja-JP": "編集準備完了", "zh-CN": "可以开工" },
  "ob.step3.desc": { en: "Green dot means every save is journaled and safe.", "de-DE": "Der grüne Punkt bedeutet: jedes Speichern ist sicher protokolliert.", "ja-JP": "緑のドットは「すべての保存が記録され保護されている」の意味。", "zh-CN": "绿灯亮起即每次保存都有日志保护。" },
  "ob.cta": { en: "Open project folder…", "de-DE": "Projektordner öffnen…", "ja-JP": "プロジェクトフォルダーを開く…", "zh-CN": "打开项目文件夹…" },

  "card.sync": { en: "Sync state", "de-DE": "Sync-Status", "ja-JP": "同期状態", "zh-CN": "同步状态" },
  "card.i1": { en: "Hydration (I1)", "de-DE": "Hydration (I1)", "ja-JP": "ハイドレーション (I1)", "zh-CN": "取回速度 (I1)" },
  "card.projects": { en: "Attached projects", "de-DE": "Verbundene Projekte", "ja-JP": "接続済みプロジェクト", "zh-CN": "已连接项目" },
  "card.files": { en: "Project files", "de-DE": "Projektdateien", "ja-JP": "プロジェクトファイル", "zh-CN": "项目文件" },
  "card.activity": { en: "Journal activity", "de-DE": "Journal-Aktivität", "ja-JP": "ジャーナル履歴", "zh-CN": "日志动态" },
  "card.review": { en: "Client review", "de-DE": "Kunden-Review", "ja-JP": "クライアントレビュー", "zh-CN": "客户审阅" },
  "card.team": { en: "Team", "de-DE": "Team", "ja-JP": "チーム", "zh-CN": "团队" },
  "card.locks": { en: "Active locks", "de-DE": "Aktive Sperren", "ja-JP": "アクティブなロック", "zh-CN": "活动锁定" },
  "card.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "card.pins": { en: "Pins", "de-DE": "Angeheftete Dateien", "ja-JP": "ピン留め", "zh-CN": "置顶文件" },
  "card.recall": { en: "Recall jobs", "de-DE": "Abruf-Jobs", "ja-JP": "リコールジョブ", "zh-CN": "取回任务" },
  "card.storage": { en: "Local storage", "de-DE": "Lokaler Speicher", "ja-JP": "ローカルストレージ", "zh-CN": "本地存储" },
  "card.flags": { en: "Kill switches", "de-DE": "Not-Schalter", "ja-JP": "キルスイッチ", "zh-CN": "运行开关" },
  "card.doctor": { en: "Doctor", "de-DE": "Arzt", "ja-JP": "ドクター", "zh-CN": "诊断" },

  "stat.pending": { en: "outbox pending", "de-DE": "Outbox offen", "ja-JP": "送信待ち", "zh-CN": "待发块" },
  "stat.cursor": { en: "journal cursor", "de-DE": "Journal-Cursor", "ja-JP": "ジャーナル位置", "zh-CN": "日志游标" },
  "stat.files": { en: "files known", "de-DE": "bekannte Dateien", "ja-JP": "既知ファイル数", "zh-CN": "已知文件" },
  "stat.conflicts": { en: "conflict copies", "de-DE": "Konflikt-Kopien", "ja-JP": "競合コピー", "zh-CN": "冲突副本" },
  "stat.i1": { en: "first byte, cached", "de-DE": "erstes Byte, gecacht", "ja-JP": "先頭バイト（キャッシュ）", "zh-CN": "首字节（缓存）" },
  "stat.chunks": { en: "local chunks", "de-DE": "lokale Chunks", "ja-JP": "ローカルチャンク", "zh-CN": "本地块" },
  "stat.cached": { en: "bytes cached", "de-DE": "Bytes gecacht", "ja-JP": "キャッシュ量", "zh-CN": "缓存字节" },
  "stat.pinned": { en: "pinned chunks", "de-DE": "angeheftete Chunks", "ja-JP": "ピン留めチャンク", "zh-CN": "置顶块" },

  "th.project": { en: "project", "de-DE": "Projekt", "ja-JP": "プロジェクト", "zh-CN": "项目" },
  "th.root": { en: "root", "de-DE": "Wurzel", "ja-JP": "ルート", "zh-CN": "根目录" },
  "th.state": { en: "state", "de-DE": "Status", "ja-JP": "状態", "zh-CN": "状态" },
  "th.files": { en: "files", "de-DE": "Dateien", "ja-JP": "ファイル", "zh-CN": "文件" },
  "th.outbox": { en: "outbox", "de-DE": "Outbox", "ja-JP": "送信箱", "zh-CN": "发件" },
  "th.lastError": { en: "last error", "de-DE": "Letzter Fehler", "ja-JP": "直近のエラー", "zh-CN": "最近错误" },
  "th.actions": { en: "actions", "de-DE": "Aktionen", "ja-JP": "操作", "zh-CN": "操作" },
  "th.file": { en: "file", "de-DE": "Datei", "ja-JP": "ファイル", "zh-CN": "文件" },
  "th.size": { en: "size", "de-DE": "Größe", "ja-JP": "サイズ", "zh-CN": "大小" },
  "th.sync": { en: "sync", "de-DE": "Sync", "ja-JP": "同期", "zh-CN": "同步" },
  "th.badges": { en: "state", "de-DE": "Status", "ja-JP": "状態", "zh-CN": "状态" },
  "th.seq": { en: "seq", "de-DE": "Seq", "ja-JP": "シーケンス", "zh-CN": "序号" },
  "th.path": { en: "path", "de-DE": "Pfad", "ja-JP": "パス", "zh-CN": "路径" },
  "th.token": { en: "token", "de-DE": "Token", "ja-JP": "トークン", "zh-CN": "令牌" },
  "th.expires": { en: "expires", "de-DE": "Läuft ab", "ja-JP": "期限", "zh-CN": "到期" },
  "th.commit": { en: "commit", "de-DE": "Commit", "ja-JP": "コミット", "zh-CN": "提交" },
  "th.label": { en: "label", "de-DE": "Label", "ja-JP": "ラベル", "zh-CN": "标签" },
  "th.author": { en: "author", "de-DE": "Autor", "ja-JP": "作成者", "zh-CN": "作者" },
  "th.member": { en: "member", "de-DE": "Mitglied", "ja-JP": "メンバー", "zh-CN": "成员" },
  "th.role": { en: "role", "de-DE": "Rolle", "ja-JP": "役割", "zh-CN": "角色" },
  "th.action": { en: "action", "de-DE": "Aktion", "ja-JP": "アクション", "zh-CN": "操作" },
  "th.when": { en: "when", "de-DE": "Wann", "ja-JP": "日時", "zh-CN": "时间" },

  "empty.projects": { en: "No attached projects — attach a root above or via cairn attach.", "de-DE": "Keine verbundenen Projekte — oben eine Wurzel verbinden oder cairn attach nutzen.", "ja-JP": "接続済みプロジェクトはありません — 上でルートを接続するか cairn attach を使ってください。", "zh-CN": "暂无已连接项目 — 在上方连接或使用 cairn attach。" },
  "empty.files": { en: "No files yet — attach a project and drop media in.", "de-DE": "Noch keine Dateien — Projekt verbinden und Medien ablegen.", "ja-JP": "ファイルはまだありません — プロジェクトを接続してメディアを置いてください。", "zh-CN": "暂无文件 — 连接项目后放入媒体。" },
  "empty.activity": { en: "No entries yet — saves appear here as they are journaled.", "de-DE": "Noch keine Einträge — Speicherungen erscheinen hier, sobald sie protokolliert sind.", "ja-JP": "エントリはまだありません — 保存が記録されるとここに表示されます。", "zh-CN": "暂无记录 — 保存入日志后会显示在这里。" },
  "empty.locks": { en: "No live locks on this machine.", "de-DE": "Keine aktiven Sperren auf dieser Maschine.", "ja-JP": "このマシンにアクティブなロックはありません。", "zh-CN": "本机暂无活动锁定。" },
  "empty.versions": { en: "No versions yet — create one after the first sync.", "de-DE": "Noch keine Versionen — nach dem ersten Sync eine erstellen.", "ja-JP": "バージョンはまだありません — 初回同期後に作成してください。", "zh-CN": "暂无版本 — 首次同步后创建一个。" },
  "empty.pins": { en: "No pins on this machine.", "de-DE": "Keine Pins auf dieser Maschine.", "ja-JP": "このマシンにピンはありません。", "zh-CN": "本机暂无置顶。" },
  "empty.recall": { en: "No recall jobs yet.", "de-DE": "Noch keine Abruf-Jobs.", "ja-JP": "リコールジョブはまだありません。", "zh-CN": "暂无取回任务。" },
  "empty.team": { en: "No project attached — the team roster syncs with the project.", "de-DE": "Kein Projekt verbunden — die Teamliste synchronisiert mit dem Projekt.", "ja-JP": "プロジェクトが未接続 — チーム名簿はプロジェクトと同期されます。", "zh-CN": "未连接项目 — 团队名册随项目同步。" },
  "card.live": { en: "Live presence", "de-DE": "Live-Präsenz", "ja-JP": "ライブプレゼンス", "zh-CN": "实时在线" },
  "note.live": { en: "Other editors' playheads, streamed in real time. Off by default — each editor opts in via the live_presence flag.", "de-DE": "Playheads anderer Editoren in Echtzeit. Standardmäßig aus — jede:r Editor:in aktiviert das Flag live_presence selbst.", "ja-JP": "他の編集者のプレイヘッドをリアルタイム表示。デフォルトはオフ — live_presence フラグで各自が有効化します。", "zh-CN": "实时显示其他剪辑的播放头。默认关闭 — 每位剪辑师自行开启 live_presence 标志。" },
  "empty.live": { en: "No editors online right now.", "de-DE": "Gerade keine Editoren online.", "ja-JP": "現在オンラインの編集者はいません。", "zh-CN": "当前没有编辑在线。" },
  "live.off": { en: "Live presence is OFF on this device — flip the live_presence flag (applies at next swarm join).", "de-DE": "Live-Präsenz ist auf diesem Gerät AUS — live_presence-Flag umschalten (greift beim nächsten Swarm-Join).", "ja-JP": "この端末ではライブプレゼンスはオフ — live_presence フラグを切り替えてください（次回の swarm 参加時に有効）。", "zh-CN": "本设备的实时在线已关闭 — 切换 live_presence 标志（下次加入 swarm 时生效）。" },
  "empty.review": { en: "no review sessions — publish one: cairn review publish --media cuts/v1.mp4", "de-DE": "keine Review-Sitzungen — veröffentliche eine: cairn review publish --media cuts/v1.mp4", "ja-JP": "レビューセッションはありません — 公開するには: cairn review publish --media cuts/v1.mp4", "zh-CN": "暂无审阅会话 — 发布一个：cairn review publish --media cuts/v1.mp4" },

  "files.filter": { en: "filter by name…", "de-DE": "nach Name filtern…", "ja-JP": "名前で絞り込み…", "zh-CN": "按名称筛选…" },
  "files.summary": {
    en: "{files} files — {synced} synced · {syncing} syncing · {conflict} conflict",
    "de-DE": "{files} Dateien — {synced} synchron · {syncing} läuft · {conflict} Konflikt",
    "ja-JP": "{files} ファイル — 同期済み {synced} · 同期中 {syncing} · 競合 {conflict}",
    "zh-CN": "{files} 个文件 — 已同步 {synced} · 同步中 {syncing} · 冲突 {conflict}"
  },
  "files.local": { en: "local", "de-DE": "lokal", "ja-JP": "ローカル", "zh-CN": "本地" },
  "files.synced": { en: "synced", "de-DE": "synchron", "ja-JP": "同期済み", "zh-CN": "已同步" },
  "files.syncing": { en: "syncing", "de-DE": "synchronisiert", "ja-JP": "同期中", "zh-CN": "同步中" },
  "files.conflict": { en: "conflict", "de-DE": "Konflikt", "ja-JP": "競合", "zh-CN": "冲突" },
  "files.pinned": { en: "pinned", "de-DE": "angepinnt", "ja-JP": "ピン留め", "zh-CN": "已置顶" },
  "files.placeholder": { en: "placeholder", "de-DE": "Platzhalter", "ja-JP": "プレースホルダー", "zh-CN": "占位文件" },
  "files.error": { en: "couldn’t reach the daemon — the list fills in the moment it responds", "de-DE": "Daemon nicht erreichbar — die Liste erscheint, sobald er antwortet", "ja-JP": "デーモンに接続できません — 応答すると一覧が表示されます", "zh-CN": "无法连接守护进程 — 响应后列表会自动出现" },

  "team.myRole": { en: "your role", "de-DE": "deine Rolle", "ja-JP": "あなたの役割", "zh-CN": "你的角色" },
  "team.invite": { en: "invite a machine — join code", "de-DE": "Maschine einladen — Beitrittscode", "ja-JP": "マシンを招待 — 参加コード", "zh-CN": "邀请设备 — 加入码" },
  "team.audit": { en: "Recent decisions (synced audit ledger)", "de-DE": "Letzte Entscheidungen (synchronisiertes Audit-Log)", "ja-JP": "最近の判断（同期される監査台帳）", "zh-CN": "最近的权限判定（随项目同步的审计账）" },
  "team.allowed": { en: "allowed", "de-DE": "erlaubt", "ja-JP": "許可", "zh-CN": "允许" },
  "team.denied": { en: "denied", "de-DE": "verweigert", "ja-JP": "拒否", "zh-CN": "拒绝" },

  "chip.ok": { en: "all files synced", "de-DE": "alle Dateien synchron", "ja-JP": "全ファイル同期済み", "zh-CN": "全部文件已同步" },
  "chip.warn": { en: "degraded", "de-DE": "beeinträchtigt", "ja-JP": "縮小運転", "zh-CN": "降级" },
  "chip.bad": { en: "daemon unreachable", "de-DE": "Daemon nicht erreichbar", "ja-JP": "デーモンに到達できません", "zh-CN": "守护进程不可达" },
  "chip.update": { en: "update available", "de-DE": "Update verfügbar", "ja-JP": "アップデートあり", "zh-CN": "有可用更新" },
  "chip.updateFailed": { en: "update check failed", "de-DE": "Update-Prüfung fehlgeschlagen", "ja-JP": "アップデート確認に失敗", "zh-CN": "更新检查失败" },

  "quota.warn": { en: "store volume is above 95% — archive or evict before sync stalls", "de-DE": "Speichervolumen über 95% — archivieren oder räumen, bevor der Sync stockt", "ja-JP": "ストア領域が95%超 — 同期が止まる前にアーカイブ/整理を", "zh-CN": "存储卷已超 95% — 请归档或清理，避免同步停滞" },
  "quota.note": { en: "used of the store volume", "de-DE": "belegt vom Speichervolumen", "ja-JP": "ストア領域の使用量", "zh-CN": "存储卷已用" },

  "help.title": { en: "Shortcuts & cheatsheet", "de-DE": "Kürzel & Spickzettel", "ja-JP": "ショートカットとチートシート", "zh-CN": "快捷键与速查表" },
  "help.keys": { en: "Keys", "de-DE": "Tasten", "ja-JP": "キー", "zh-CN": "按键" },
  "help.cli": { en: "CLI cheatsheet", "de-DE": "CLI-Spickzettel", "ja-JP": "CLIチートシート", "zh-CN": "命令速查" },
  "help.states": { en: "What the dot means", "de-DE": "Was der Punkt bedeutet", "ja-JP": "ドットの意味", "zh-CN": "状态点含义" },
  "help.search": { en: "focus search", "de-DE": "Suche fokussieren", "ja-JP": "検索にフォーカス", "zh-CN": "聚焦搜索" },
  "help.help": { en: "toggle this panel", "de-DE": "dieses Panel umschalten", "ja-JP": "このパネルの切替", "zh-CN": "切换此面板" },
  "help.goOverview": { en: "go to overview", "de-DE": "zur Übersicht", "ja-JP": "概要へ", "zh-CN": "前往总览" },
  "help.goFiles": { en: "go to files", "de-DE": "zu den Dateien", "ja-JP": "ファイルへ", "zh-CN": "前往文件" },
  "help.esc": { en: "close overlays", "de-DE": "Overlays schließen", "ja-JP": "オーバーレイを閉じる", "zh-CN": "关闭浮层" },
  "help.dotOk": { en: "all files synced — safe to edit", "de-DE": "alle Dateien synchron — sicher zum Schneiden", "ja-JP": "全ファイル同期済み — 編集して安全", "zh-CN": "全部已同步 — 可安全编辑" },
  "help.dotWarn": { en: "syncing — chunks in flight", "de-DE": "synchronisiert — Chunks unterwegs", "ja-JP": "同期中 — チャンク転送中", "zh-CN": "同步中 — 数据块传输中" },
  "help.dotBad": { en: "attention — see Doctor", "de-DE": "Achtung — siehe Arzt", "ja-JP": "要注意 — ドクターを確認", "zh-CN": "需要注意 — 查看诊断" },

  "note.sync": { en: "The daemon watches attached roots, chunks content with FastCDC, and appends every save to the server-linearized journal.", "de-DE": "Der Daemon beobachtet verbundene Wurzeln, stückelt Inhalte mit FastCDC und hängt jedes Speichern ans server-linearisierte Journal.", "ja-JP": "デーモンは接続ルートを監視し、FastCDCでチャンク化し、すべての保存をサーバー線形化ジャーナルに追記します。", "zh-CN": "守护进程监视已连接的根目录，用 FastCDC 切块，并将每次保存追加到服务器线性化的日志。" },
  "note.i1": { en: "target < 50 ms · measured from the header cache", "de-DE": "Ziel < 50 ms · gemessen am Header-Cache", "ja-JP": "目標 < 50 ms · ヘッダーキャッシュから計測", "zh-CN": "目标 < 50 ms · 来自头缓存实测" },
  "note.locks": { en: "NLE project files auto-acquire locks on open; fencing tokens protect every append. \"Lock\" is the word editors already use — a lease is how it is implemented.", "de-DE": "NLE-Projektdateien sperren sich beim Öffnen automatisch; Fencing-Token schützen jeden Anhang. „Sperre“ sagt der Editor — der Lease ist die Umsetzung.", "ja-JP": "NLEプロジェクトファイルはオープン時に自動ロック。フェンシングトークンがすべての追記を保護します。", "zh-CN": "NLE 工程文件打开时自动加锁；栅栏令牌保护每次追加。「锁定」是剪辑师熟悉的词——租约是其实现。" },
  "note.versions": { en: "Versions fold from the journal every 5,000 entries, every 24h, on demand, or at project close — every fold is a restore point.", "de-DE": "Versionen falten aus dem Journal alle 5.000 Einträge, alle 24h, auf Abruf oder bei Projektende — jede Faltung ist ein Wiederherstellungspunkt.", "ja-JP": "バージョンは5,000エントリごと・24時間ごと・要求時・プロジェクト終了時にジャーナルから折りたたまれます。", "zh-CN": "版本按每 5,000 条、每 24 小时、按需或工程结束时从日志折叠生成——每次折叠都是一个还原点。" },
  "note.pins": { en: "Pinned files are recalled into local storage and exempt from LRU eviction.", "de-DE": "Angeheftete Dateien liegen lokal und sind von der LRU-Räumung ausgenommen.", "ja-JP": "ピン留めファイルはローカルに保持され、LRU退避の対象外です。", "zh-CN": "置顶文件保留在本地，且不受 LRU 淘汰影响。" },
  "note.recall": { en: "Recall materializes files from the bucket into local storage (progress per job).", "de-DE": "Abruf materialisiert Dateien aus dem Bucket in den lokalen Speicher (Fortschritt pro Job).", "ja-JP": "リコールはバケットからファイルをローカルへ実体化します（ジョブごとの進捗）。", "zh-CN": "取回将文件从桶实体化到本地存储（每个任务显示进度）。" },
  "note.storage": { en: "NLE media caches belong on local scratch; pinned chunks are never evicted.", "de-DE": "NLE-Medien-Caches gehören auf lokalen Scratch; angeheftete Chunks werden nie geräumt.", "ja-JP": "NLEメディアキャッシュはローカルスクラッチに。ピン留めチャンクは退避されません。", "zh-CN": "NLE 媒体缓存应放在本地暂存盘；置顶块永不被淘汰。" },
  "note.flags": { en: "Flags take effect on the next job run. No restart required. Owner/lead only (RBAC-enforced).", "de-DE": "Schalter greifen beim nächsten Job-Lauf. Kein Neustart. Nur Owner/Lead (RBAC-prüfung).", "ja-JP": "フラグは次のジョブ実行時に反映。再起動不要。オーナー/リード限定（RBAC強制）。", "zh-CN": "开关在下次任务运行时生效，无需重启。仅所有者/主管可用（RBAC 强制）。" },

  "btn.attach": { en: "attach", "de-DE": "verbinden", "ja-JP": "接続", "zh-CN": "连接" },
  "btn.snapshot": { en: "create version", "de-DE": "Version erstellen", "ja-JP": "バージョン作成", "zh-CN": "创建版本" },
  "btn.refresh": { en: "refresh", "de-DE": "aktualisieren", "ja-JP": "更新", "zh-CN": "刷新" },
  "btn.pin": { en: "pin", "de-DE": "anheften", "ja-JP": "ピン留め", "zh-CN": "置顶" },
  "btn.recall": { en: "start recall", "de-DE": "Abruf starten", "ja-JP": "リコール開始", "zh-CN": "开始取回" },
  "btn.copy": { en: "copy", "de-DE": "kopieren", "ja-JP": "コピー", "zh-CN": "复制" },

  "attach.root": { en: "/path/to/workspace — attach a root", "de-DE": "/pfad/zum/workspace — Wurzel verbinden", "ja-JP": "/path/to/workspace — ルートを接続", "zh-CN": "/path/to/workspace — 连接根目录" },
  "attach.project": { en: "project id (optional)", "de-DE": "Projekt-ID (optional)", "ja-JP": "プロジェクトID（任意）", "zh-CN": "项目 ID（可选）" },
  "versions.label": { en: "label (e.g. before color pass)", "de-DE": "Label (z. B. vor dem Farbdurchgang)", "ja-JP": "ラベル（例: カラー前）", "zh-CN": "标签（如：调色前）" },
  "pins.path": { en: "relative path to pin (e.g. scene.prproj)", "de-DE": "relativer Pfad (z. B. scene.prproj)", "ja-JP": "ピン留めする相対パス（例: scene.prproj）", "zh-CN": "要置顶的相对路径（如 scene.prproj）" },
  "recall.path": { en: "one path (optional — whole project if empty)", "de-DE": "ein Pfad (optional — sonst ganzes Projekt)", "ja-JP": "1パス（任意 — 空なら全プロジェクト）", "zh-CN": "单个路径（可选 — 留空则整个项目）" },
};

function detectLang() {
  const saved = localStorage.getItem("cairn-lang");
  if (saved && STR["nav.overview"][saved]) return saved;
  const nav = (navigator.language || "en").trim();
  if (STR["nav.overview"][nav]) return nav;
  const base = nav.split("-")[0];
  if (base === "de") return "de-DE";
  if (base === "ja") return "ja-JP";
  if (base === "zh") return "zh-CN";
  return "en";
}
let LANG = detectLang();

function t(key) {
  const row = STR[key];
  if (!row) return key;
  return row[LANG] || row.en || key;
}

function applyI18n() {
  document.querySelectorAll("[data-i18n]").forEach((el) => {
    const v = t(el.dataset.i18n);
    if (v && !v.includes("<")) el.textContent = v;
  });
  document.querySelectorAll("[data-i18n-attr]").forEach((el) => {
    for (const pair of el.dataset.i18nAttr.split(";")) {
      const [attr, key] = pair.split(":").map((s) => (s || "").trim());
      if (attr && key && STR[key]) el.setAttribute(attr, t(key));
    }
  });
  document.documentElement.lang = LANG;
  document.querySelectorAll(".lang").forEach((b) => {
    b.classList.toggle("is-active", b.dataset.lang === LANG);
  });
}

document.querySelectorAll(".lang").forEach((b) => {
  b.addEventListener("click", () => {
    LANG = b.dataset.lang;
    localStorage.setItem("cairn-lang", LANG);
    applyI18n();
    renderOnboarding();
    renderFiles(LAST_FILES);
    renderReview(LAST_REVIEW);
    renderLocks(LAST_LOCKS);
    renderActivity(LAST_ACTIVITY);
    refreshTeam();
    refreshStatus();
  });
});

/* ---------- safety: escape server-provided strings for innerHTML ---------- */

function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

/* ---------- formatting ---------- */

function fmtBytes(n) {
  if (!Number.isFinite(n)) return "—";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  let v = n;
  while (v >= 1024 && i < units.length - 1) { v /= 1024; i += 1; }
  return `${v.toFixed(v >= 100 || i === 0 ? 0 : 1)} ${units[i]}`;
}

function fmtUptime(ms) {
  const s = Math.floor(ms / 1000);
  const m = Math.floor(s / 60);
  const h = Math.floor(m / 60);
  if (h > 0) return `uptime ${h}h ${m % 60}m`;
  if (m > 0) return `uptime ${m}m ${s % 60}s`;
  return `uptime ${s}s`;
}

function fmtWhen(ms) {
  if (!Number.isFinite(ms) || ms <= 0) return "";
  const d = new Date(ms);
  return d.toLocaleString(undefined, { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit" });
}

function relTime(ms) {
  if (!Number.isFinite(ms) || ms <= 0) return "";
  const d = Date.now() - ms;
  if (d < 0) return "";
  if (d < 60e3) return "just now";
  if (d < 3600e3) return Math.floor(d / 60e3) + "m ago";
  if (d < 86400e3) return Math.floor(d / 3600e3) + "h ago";
  return Math.floor(d / 86400e3) + "d ago";
}

function setChip(el, cls, label) {
  el.className = `state-chip ${cls}`;
  el.lastElementChild.textContent = label;
}

async function getJSON(url) {
  const res = await fetch(url, { headers: { Accept: "application/json" } });
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.json();
}

async function postJSON(url, body) {
  const res = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
  return res.json();
}

/* ---------- selected project (shared by files/snapshot/pin/recall) ---------- */

let PROJECTS = [];
let HEALTHY = false;

function selectedProject(selectId) {
  const el = $(selectId);
  if (el && el.value) return el.value;
  return PROJECTS.length > 0 ? PROJECTS[0].project_id : "";
}

function fillProjectSelects() {
  for (const id of ["files-project", "snapshot-project", "pin-project", "recall-project"]) {
    const el = $(id);
    if (!el) continue;
    const prev = el.value;
    el.innerHTML = "";
    for (const p of PROJECTS) {
      const opt = document.createElement("option");
      opt.value = p.project_id;
      opt.textContent = p.display_name ? `${p.display_name} (${p.project_id})` : p.project_id;
      el.appendChild(opt);
    }
    if (prev && PROJECTS.some((p) => p.project_id === prev)) el.value = prev;
  }
}

/* ---------- onboarding: the EMPTY-STATE hero only ----------
   The design-review verdict was blunt and right: a three-step wizard
   above the sync metrics treats a DIT like a novice, and a project that
   is already attached does not need a wizard. So: zero roots -> the full
   welcome hero (audit #1); anything attached -> the card hides and the
   state chip + files meter carry the story. */

function renderOnboarding() {
  const card = $("onboarding");
  if (!card) return;
  const attached = PROJECTS.length > 0;

  if (attached) {
    card.hidden = true;
    return;
  }
  card.hidden = false;
  card.classList.add("hero");
  $("ob-title").textContent = t("ob.title.welcome");
  $("ob-sub").textContent = t("ob.sub.welcome");

  const setStep = (id, state) => {
    const el = $(id);
    el.classList.remove("done", "now", "todo");
    el.classList.add(state);
    el.querySelector(".ob-num").textContent =
      state === "done" ? "\u2713" : id.slice(-1);
  };
  setStep("ob-step-1", "now");
  setStep("ob-step-2", "todo");
  setStep("ob-step-3", "todo");
  const note = $("ob-note");
  note.textContent = "";

  // footer track (72x3) over the real state machine, never faked: the hero
  // only exists at zero roots, so the track honestly sits at 1/3 — attach
  // a root (button or CLI) and the next poll replaces this card entirely.
  const stage = 1;
  const fill = $("ob-track-fill");
  const track = $("ob-track");
  const label = $("ob-stage-label");
  if (fill) fill.style.width = `${(stage / 3) * 100}%`;
  if (track) track.setAttribute("aria-valuenow", String(stage));
  if (label) label.textContent = `${stage} / 3 · ${t("ob.step1.name")}`;
}

/* ---------- status header + overview ---------- */

async function refreshStatus() {
  try {
    const s = await getJSON("/api/v1/status");
    $("daemon-version").textContent = `v${s.version}`;
    $("daemon-proto").textContent = `v${s.proto}`;
    $("daemon-uptime").textContent = fmtUptime(s.uptime_ms);

    const summary = s.summary || {};
    const healthy = summary.healthy === true;
    HEALTHY = healthy;
    setChip(
      $("state-chip"),
      healthy ? "is-ok" : "is-warn",
      healthy ? t("chip.ok") : t("chip.warn")
    );

    $("stat-pending").textContent = summary.outbox_pending ?? 0;
    $("stat-cursor").textContent = summary.journal_cursor ?? 0;
    $("stat-files").textContent = summary.files ?? 0;
    $("stat-conflicts").textContent = summary.conflicts ?? 0;

    const i1 = summary.hydration_first_byte_ms;
    if (Number.isFinite(i1)) {
      $("stat-i1").textContent = `${i1.toFixed(1)} ms`;
      const pct = Math.max(4, Math.min(100, (i1 / 50) * 100));
      $("i1-meter").style.width = `${pct}%`;
      $("i1-meter").style.background = i1 < 50 ? "var(--ok)" : "var(--bad)";
    } else {
      // honest null: small quiet text, not a giant glyph that reads as a
      // rendering error (design review)
      $("stat-i1").textContent = "no mount";
      $("stat-i1").classList.add("stat-null");
      $("i1-meter").style.width = "0%";
    }
  } catch {
    HEALTHY = false;
    setChip($("state-chip"), "is-bad", t("chip.bad"));
  }
}

/* ---------- local storage + quota meter (audit #8) ---------- */

async function refreshStorage() {
  try {
    const r = await getJSON("/api/v1/storage");
    if (r.ok !== true) return;
    const b = r.blobs || {};
    $("stat-blobs").textContent = b.count ?? 0;
    $("stat-bytes").textContent = fmtBytes(b.bytes ?? 0);
    $("stat-pinned").textContent = b.pinned_count ?? 0;
    const note = $("storage-note");
    const fill = $("quota-meter-fill");
    if (r.disk && Number.isFinite(r.disk.free_bytes) && Number.isFinite(r.disk.total_bytes) && r.disk.total_bytes > 0) {
      const used = Math.max(0, r.disk.total_bytes - r.disk.free_bytes);
      const pct = Math.min(100, (used / r.disk.total_bytes) * 100);
      fill.style.width = `${Math.max(2, pct)}%`;
      if (pct >= 95) {
        fill.style.background = "var(--bad)";
        fill.classList.add("pulse");
        note.textContent = t("quota.warn");
      } else {
        fill.style.background = pct >= 80 ? "var(--warn)" : "var(--ok)";
        fill.classList.remove("pulse");
        note.textContent =
          `${fmtBytes(used)} ${t("quota.note")} ${fmtBytes(r.disk.total_bytes)} — ` +
          "NLE media caches belong on local scratch; pinned chunks are never evicted.";
      }
    } else {
      fill.style.width = "0%";
    }
  } catch { /* store down: stats stay at their last honest value */ }
}

/* ---------- update chip (audit #11, honest) ---------- */

async function refreshUpdate() {
  try {
    const r = await getJSON("/api/v1/update");
    const chip = $("update-chip");
    if (!chip || r.ok !== true) return;
    if (r.update_offered) {
      chip.hidden = false;
      chip.className = "state-chip chip-update is-warn";
      $("update-label").textContent = t("chip.update");
    } else if (r.check_failed) {
      chip.hidden = false;
      chip.className = "state-chip chip-update is-bad";
      $("update-label").textContent = t("chip.updateFailed");
    } else {
      chip.hidden = true;
    }
  } catch { /* never a lie on failure: chip stays hidden */ }
}

/* ---------- activity ---------- */

function renderActivity(entries) {
  const body = $("activity-body");
  body.innerHTML = "";
  if (!entries || entries.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.activity"))}</td></tr>`;
    return;
  }
  for (const e of entries.slice(-12).reverse()) {
    const tr = document.createElement("tr");
    const kind = e.kind || "upsert";
    const tag = kind === "delete" ? "bad" : kind === "rename" ? "info" : "ok";
    tr.innerHTML =
      `<td class="num">${esc(e.seq ?? "—")}</td>` +
      `<td>${esc(e.path ?? "")}</td>` +
      `<td class="sans"><span class="tag ${tag}">${esc(kind)}</span></td>` +
      `<td class="num">${esc(fmtBytes(e.size))}</td>`;
    body.appendChild(tr);
  }
}

/* ---------- locks (renamed from leases — audit #4) ---------- */

function renderLocks(locks) {
  const body = $("lease-body");
  body.innerHTML = "";
  if (!locks || locks.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.locks"))}</td></tr>`;
    return;
  }
  const now = Date.now();
  for (const l of locks) {
    const remainMs = (l.expires_at ?? 0) - now;
    const live = remainMs > 0;
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td>${esc(l.path ?? "")}</td>` +
      `<td>${esc(l.token ?? "—")}</td>` +
      `<td class="num">${live ? `${Math.ceil(remainMs / 1000)}s` : "expired"}</td>` +
      `<td class="sans"><span class="tag ${live ? "ok" : "warn"}">${live ? "held" : "stale"}</span></td>`;
    body.appendChild(tr);
  }
}

/* ---------- projects (display names — audit #4) ---------- */

function renderProjects(projects) {
  PROJECTS = projects || [];
  fillProjectSelects();
  const body = $("project-body");
  body.innerHTML = "";
  if (!PROJECTS.length) {
    body.innerHTML = `<tr><td colspan="7" class="empty">${esc(t("empty.projects"))}</td></tr>`;
    return;
  }
  for (const p of PROJECTS) {
    const tr = document.createElement("tr");
    const stateTag =
      p.state === "error" ? "bad" : p.state === "syncing" ? "info" : "ok";
    const err = p.last_error
      ? `<span class="tag bad">error</span> ${esc(p.last_error)}`
      : "—";
    const nameCell = p.display_name
      ? `<div class="proj-name">${esc(p.display_name)}</div><div class="proj-id">${esc(p.project_id)}</div>`
      : esc(p.project_id);
    tr.innerHTML =
      `<td class="sans">${nameCell}</td>` +
      `<td>${esc(p.root_path ?? "")}</td>` +
      `<td class="sans"><span class="tag ${stateTag}">${esc(p.state ?? "?")}</span></td>` +
      `<td class="num">${esc(p.files_synced ?? 0)}</td>` +
      `<td class="num">${esc(p.pending_outbox ?? 0)}</td>` +
      `<td>${err}</td>` +
      `<td class="sans"><button type="button" class="btn btn-ghost" data-detach="${esc(p.project_id)}">detach</button></td>`;
    tr.querySelector("[data-detach]").addEventListener("click", async (ev) => {
      if (!confirm(`Detach ${ev.currentTarget.dataset.detach}? Local files stay.`)) return;
      const r = await postJSON("/api/v1/detach", { project_id: ev.currentTarget.dataset.detach });
      if (r && r.ok === false) alert(`detach denied: ${r.error}`);
      refreshAll();
    });
    body.appendChild(tr);
  }
}

async function refreshProjects() {
  try {
    const r = await getJSON("/api/v1/projects");
    renderProjects(r.projects);
    const attached = PROJECTS.length;
    const first = PROJECTS[0];
    $("project-name").textContent =
      attached === 0
        ? t("head.noRoots")
        : attached === 1
          ? (first && (first.display_name || first.root_path)) || first.project_id
          : `${attached} roots attached`;
  } catch { /* covered by status chip */ }
}

/* ---------- files (audit #2: Finder-style overlays, honest loading) ----------
   The list is a table, but the state column carries 16px overlay icons —
   cloud + a status pip, the way an editor already reads file state in a
   file browser. Skeleton rows shimmer while the first load is in flight;
   a failed first load shows the mark + a plain sentence (never a spinny
   nothing). Once data exists, a failed refresh keeps the last honest rows. */

/* 16px overlay icons — cloud outline + corner pip (currentColor + pip color) */
const F_CLOUD =
  '<svg class="f-ic" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" aria-hidden="true">' +
  '<path d="M4.6 10.9a2.1 2.1 0 0 1-.2-4.2 3.5 3.5 0 0 1 6.8-.7 2.3 2.3 0 0 1-.3 4.9z" stroke-width="1.2" stroke-linejoin="round"/>' +
  '<circle cx="12.2" cy="11.6" r="3.4" class="ov" stroke="none"/>{MARK}</svg>';
const F_DOC =
  '<svg class="f-ic f-doc" width="16" height="16" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.1" aria-hidden="true">' +
  '<path d="M4 1.8h5.2L12 4.6V14.2H4z" stroke-linejoin="round"/><path d="M9.2 1.8v2.8H12" stroke-linejoin="round"/></svg>';
const F_MARKS = {
  synced: '<path d="M10.9 11.6l1.3 1.3 2.2-2.3" class="ov-stroke" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round" stroke-linejoin="round"/>',
  syncing: '<path d="M12.2 10.3v2.6M11 11.2l1.2-1.2 1.2 1.2" class="ov-stroke" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round" stroke-linejoin="round"/>',
  conflict: '<path d="M12.2 10.4v1.6" class="ov-stroke" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/><circle cx="12.2" cy="13.1" r="0.5" fill="currentColor" stroke="none"/>',
  placeholder: '<path d="M11 11.6h2.4" class="ov-stroke" stroke="currentColor" stroke-width="1.4" fill="none" stroke-linecap="round"/>',
};

function fileIcon(st) {
  return F_CLOUD.replace("{MARK}", F_MARKS[st] || F_MARKS.syncing);
}

/* shimmer skeleton rows — widths vary so it reads as a list, not a ladder */
function renderFilesSkeleton() {
  const body = $("files-body");
  body.innerHTML = "";
  for (let i = 0; i < 6; i++) {
    const tr = document.createElement("tr");
    tr.className = "f-skel";
    tr.innerHTML =
      `<td><span class="skel" style="width:${46 + ((i * 29) % 38)}%; display:inline-block"></span></td>` +
      `<td class="num"><span class="skel" style="width:34px; display:inline-block"></span></td>` +
      `<td class="sans"><span class="skel" style="width:70px; display:inline-block"></span></td>` +
      `<td class="sans"><span class="skel" style="width:${72 + ((i * 17) % 30)}px; display:inline-block"></span></td>`;
    body.appendChild(tr);
  }
}

/* the 76px fallback mark — the stack, dimmed, plus one plain sentence */
function renderFilesError() {
  const body = $("files-body");
  body.innerHTML =
    `<tr><td colspan="4" class="f-error">` +
    `<svg class="f-error-mark" width="76" height="76" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">` +
    `<rect x="9" y="3.5" width="6" height="4" rx="1.2" fill="currentColor" opacity="0.35"/>` +
    `<rect x="6.5" y="9" width="11" height="4" rx="1.2" fill="currentColor" opacity="0.5"/>` +
    `<rect x="3.5" y="14.5" width="17" height="4" rx="1.2" fill="currentColor" opacity="0.65"/></svg>` +
    `<p class="f-error-note">${esc(t("files.error"))}</p></td></tr>`;
}

function renderFiles(r) {
  const body = $("files-body");
  const sum = $("files-summary");
  const fill = $("files-meter-fill");
  if (!body) return;
  body.innerHTML = "";
  if (!r || r.ok !== true || !r.files || r.files.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.files"))}</td></tr>`;
    sum.textContent = "";
    fill.style.width = "0%";
    return;
  }
  const s = r.summary || {};
  sum.textContent = t("files.summary")
    .replace("{files}", String(s.files ?? 0))
    .replace("{synced}", String(s.synced ?? 0))
    .replace("{syncing}", String(s.syncing ?? 0))
    .replace("{conflict}", String(s.conflict ?? 0));
  const total = Math.max(1, Number(s.files) || 1);
  const syncedN = Number(s.synced) || 0;
  fill.style.width = `${Math.max(2, Math.round((syncedN / total) * 100))}%`;
  fill.style.background = (Number(s.conflict) || 0) > 0 ? "var(--warn)" : "var(--ok)";

  for (const f of r.files.slice(0, 300)) {
    const tr = document.createElement("tr");
    const st = f.state || "syncing";
    const tagCls = st === "conflict" ? "bad" : st === "synced" ? "ok" : "info";
    const extras =
      (f.pinned ? `<span class="tag pin">${esc(t("files.pinned"))}</span>` : "") +
      (f.placeholder ? `<span class="tag info">${esc(t("files.placeholder"))}</span>` : "");
    tr.innerHTML =
      `<td class="sans"><span class="f-name">${F_DOC}${esc(f.path)}</span></td>` +
      `<td class="num">${esc(fmtBytes(f.size))}</td>` +
      `<td class="sans"><span class="f-state ${tagCls}">${fileIcon(st)}<span class="tag ${tagCls}">${esc(t("files." + st))}</span></span></td>` +
      `<td class="sans">${extras || '<span class="dim">—</span>'}</td>`;
    body.appendChild(tr);
  }
}

async function refreshFiles() {
  const project = selectedProject("files-project");
  if (!project) { LAST_FILES = null; renderFiles(null); return; }
  const filter = $("files-filter").value.trim();
  const key = `${project}::${filter}`;
  if (FILES_KEY !== key) {
    FILES_KEY = key;
    renderFilesSkeleton();   // new surface, no data yet — shimmer, don't lie
  }
  const q = filter ? `&q=${encodeURIComponent(filter)}` : "";
  try {
    LAST_FILES = await getJSON(`/api/v1/files?project=${encodeURIComponent(project)}${q}`);
    renderFiles(LAST_FILES);
  } catch {
    // no data yet -> the honest fallback mark; data exists -> keep the last
    // truthful rows rather than blanking to an error the user can't act on
    if (!LAST_FILES || LAST_FILES.ok !== true) renderFilesError();
  }
}

/* ---------- team (audit #5: members, my role, invite, audit ledger) ---------- */

function renderTeam(r) {
  const body = $("team-body");
  body.textContent = "";
  const projects = (r && r.projects) || [];
  if (!projects.length) {
    const empty = document.createElement("div");
    empty.className = "muted-note";
    empty.textContent = t("empty.team");
    body.appendChild(empty);
    return;
  }
  for (const p of projects) {
    const card = document.createElement("div");
    card.className = "team-project";

    // roster
    const roster = document.createElement("div");
    roster.className = "team-roster";
    const me = document.createElement("div");
    me.className = "team-me";
    me.innerHTML =
      `<span class="role-chip me">${esc(t("team.myRole"))}: <b>${esc(p.my_role)}</b></span>` +
      `<span class="team-device mono">${esc(p.my_device)}</span>`;
    roster.appendChild(me);

    const table = document.createElement("table");
    table.className = "table";
    table.innerHTML =
      `<thead><tr><th>${esc(t("th.member"))}</th><th>${esc(t("th.role"))}</th><th>${esc(t("th.path"))}</th></tr></thead>` +
      "<tbody></tbody>";
    const rows = (p.members || []).slice().sort((a, b) => (a.is_me === b.is_me ? a.name.localeCompare(b.name) : a.is_me ? -1 : 1));
    for (const m of rows) {
      const tr = document.createElement("tr");
      tr.innerHTML =
        `<td class="sans">${m.is_me ? '<span class="me-mark">you</span> ' : ""}${esc(m.name || m.device_id)}</td>` +
        `<td class="sans"><span class="role-chip ${m.is_me ? "me" : ""}">${esc(m.role)}</span></td>` +
        `<td class="mono dim">${esc(m.device_id)}</td>`;
      table.querySelector("tbody").appendChild(tr);
    }
    if (!rows.length) {
      table.querySelector("tbody").innerHTML =
        `<tr><td colspan="3" class="empty">${esc(t("empty.team"))}</td></tr>`;
    }
    roster.appendChild(table);
    card.appendChild(roster);

    // invite: join code + signal
    if (p.join_code) {
      const invite = document.createElement("div");
      invite.className = "team-invite";
      invite.innerHTML =
        `<span class="invite-label">${esc(t("team.invite"))}</span>` +
        `<code class="join-code">${esc(p.join_code)}</code>` +
        `<button type="button" class="btn btn-ghost btn-copy" data-copy="${esc(p.join_code)}">${esc(t("btn.copy"))}</button>`;
      invite.querySelector(".btn-copy").addEventListener("click", copyBtn);
      card.appendChild(invite);
    }

    // audit ledger (synced — the log is not fiction)
    if (p.audit && p.audit.length) {
      const audit = document.createElement("div");
      audit.className = "team-audit";
      audit.innerHTML = `<p class="audit-h">${esc(t("team.audit"))}</p>`;
      const list = document.createElement("ul");
      list.className = "audit-list";
      for (const e of p.audit) {
        const li = document.createElement("li");
        const allow = e.allowed === true;
        li.innerHTML =
          `<span class="tag ${allow ? "ok" : "bad"}">${esc(allow ? t("team.allowed") : t("team.denied"))}</span>` +
          `<span class="mono audit-action">${esc(e.action)}</span>` +
          `<span class="dim">${esc(e.device)} · ${esc(e.role)}</span>` +
          `<span class="mono dim">${esc(relTime(e.ts_ms))}</span>`;
        list.appendChild(li);
      }
      audit.appendChild(list);
      card.appendChild(audit);
    }
    body.appendChild(card);
  }
}

async function refreshTeam() {
  try {
    const r = await getJSON("/api/v1/team");
    renderTeam(r);
  } catch { /* team stays at its last honest value */ }
}

/* ---------- search (audit #9) ---------- */

let searchTimer = null;

async function runSearch(q) {
  if (!q.trim()) { $("search-drop").hidden = true; return; }
  try {
    const r = await getJSON(`/api/v1/search?q=${encodeURIComponent(q)}`);
    const drop = $("search-drop");
    const results = (r && r.results) || [];
    if (!results.length) {
      drop.innerHTML = `<div class="sr-none">no matches for “${esc(q)}”</div>`;
    } else {
      drop.innerHTML = "";
      for (const s of results.slice(0, 12)) {
        const row = document.createElement("button");
        row.type = "button";
        row.className = "sr-row";
        row.innerHTML =
          `<span class="sr-kind ${esc(s.kind)}">${esc(s.kind)}</span>` +
          `<span class="sr-label">${esc(s.label)}</span>` +
          `<span class="sr-sub">${esc(s.sub ?? "")}</span>`;
        row.addEventListener("click", () => {
          drop.hidden = true;
          $("search-input").value = "";
          const target = document.querySelector(s.target || "#overview");
          if (target) {
            target.scrollIntoView({ behavior: "smooth", block: "start" });
            navActivate(s.target);
          }
          if (s.kind === "file" && $("files-project")) {
            const sel = $("files-project");
            if (PROJECTS.some((p) => p.project_id === s.project)) sel.value = s.project;
            refreshFiles();
          }
        });
        drop.appendChild(row);
      }
    }
    drop.hidden = false;
  } catch { /* search is best-effort */ }
}

function navActivate(href) {
  document.querySelectorAll(".nav-item").forEach((x) => {
    x.classList.toggle("is-active", x.getAttribute("href") === href);
  });
}

$("search-input").addEventListener("input", (ev) => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(() => runSearch(ev.target.value), 220);
});
$("search-input").addEventListener("keydown", (ev) => {
  if (ev.key === "Escape") { $("search-drop").hidden = true; ev.target.blur(); }
});
document.addEventListener("click", (ev) => {
  if (!ev.target.closest("#search")) $("search-drop").hidden = true;
});

/* ---------- versions (renamed from snapshots — audit #4) ---------- */

function renderSnapshots(snapshots) {
  const body = $("snapshot-body");
  body.innerHTML = "";
  if (!snapshots || snapshots.length === 0) {
    body.innerHTML = `<tr><td colspan="5" class="empty">${esc(t("empty.versions"))}</td></tr>`;
    return;
  }
  for (const s of snapshots.slice(0, 10)) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td>${esc((s.commit_hash || "").slice(0, 12))}</td>` +
      `<td class="sans">${esc(s.label || "")}</td>` +
      `<td class="num">${esc(s.snapshot_seq ?? "—")}</td>` +
      `<td class="sans">${esc(s.author || "")}</td>` +
      `<td class="sans"><button type="button" class="btn btn-ghost" data-restore="${esc(s.commit_hash)}">restore</button></td>`;
    tr.querySelector("[data-restore]").addEventListener("click", async (ev) => {
      const project = selectedProject("snapshot-project");
      if (!project) return alert("attach a project first");
      if (!confirm("Restore this version into the workspace?")) return;
      const r = await postJSON("/api/v1/snapshots/restore", {
        project_id: project,
        commit_hash: ev.currentTarget.dataset.restore,
      });
      if (r.ok) alert(`Restored ${r.restored_files} files (${fmtBytes(r.bytes)})`);
      else alert(`Restore failed: ${r.error}`);
      refreshAll();
    });
    body.appendChild(tr);
  }
}

async function refreshSnapshots() {
  const project = selectedProject("snapshot-project");
  if (!project) return;
  try {
    const r = await getJSON(`/api/v1/snapshots?project=${encodeURIComponent(project)}`);
    renderSnapshots(r.ok ? r.snapshots : []);
  } catch { /* server may be down; empty stays honest */ }
}

/* ---------- pins ---------- */

function renderPins(pins) {
  const body = $("pin-body");
  body.innerHTML = "";
  if (!pins || pins.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.pins"))}</td></tr>`;
    return;
  }
  for (const p of pins) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td>${esc(p.path)}</td>` +
      `<td class="num">${esc(fmtBytes(p.size))}</td>` +
      `<td class="sans"><span class="tag ok">${esc(p.state || "pinned")}</span></td>` +
      `<td class="sans"><button type="button" class="btn btn-ghost" data-unpin="${esc(p.path)}">unpin</button></td>`;
    tr.querySelector("[data-unpin]").addEventListener("click", async (ev) => {
      const project = selectedProject("pin-project");
      if (!project) return;
      await postJSON("/api/v1/pins/unpin", { project_id: project, path: ev.currentTarget.dataset.unpin });
      refreshPins();
    });
    body.appendChild(tr);
  }
}

async function refreshPins() {
  const project = selectedProject("pin-project");
  if (!project) return;
  try {
    const r = await getJSON(`/api/v1/pins?project=${encodeURIComponent(project)}`);
    renderPins(r.ok ? r.pins : []);
  } catch { /* covered */ }
}

/* ---------- recall jobs (progress) ---------- */

const RECALL_JOBS = new Map();

function renderRecallJobs() {
  const box = $("recall-jobs");
  if (RECALL_JOBS.size === 0) {
    box.innerHTML = `<p class="note">${esc(t("empty.recall"))}</p>`;
    return;
  }
  box.innerHTML = "";
  for (const [id, j] of RECALL_JOBS.entries()) {
    const div = document.createElement("div");
    div.className = "recall-job";
    const tag = j.state === "failed" ? "bad" : j.state === "completed" ? "ok" : "info";
    div.innerHTML =
      `<div class="recall-head"><span style="font-family:var(--mono)">${esc(id.slice(0, 8))}</span>` +
      `<span class="tag ${tag}">${esc(j.state)}</span></div>` +
      `<div class="meter"><div class="meter-fill" style="width:${Math.max(4, Math.round((j.progress || 0) * 100))}%"></div></div>`;
    box.appendChild(div);
  }
}

async function pollRecallJobs() {
  for (const [id, j] of RECALL_JOBS.entries()) {
    if (j.state === "completed" || j.state === "failed") continue;
    try {
      const r = await getJSON(`/api/v1/recall/${encodeURIComponent(id)}`);
      if (r.ok) { RECALL_JOBS.set(id, r); }
    } catch { /* keep last state */ }
  }
  renderRecallJobs();
}

/* ---------- flags ---------- */

function renderFlags(flags) {
  const grid = $("flag-grid");
  grid.innerHTML = "";
  for (const f of flags || []) {
    const on = String(f.value).toLowerCase() !== "false";
    const div = document.createElement("div");
    div.className = "flag";
    div.innerHTML =
      `<span class="flag-name">${esc(f.name)}</span>` +
      `<button type="button" data-name="${esc(f.name)}" data-next="${on ? "false" : "true"}">` +
      `${f.name === "placeholder_driver" ? esc(f.value) : on ? "enabled" : "disabled"}</button>`;
    div.querySelector("button").addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      const r = await postJSON("/api/v1/flags", { name: btn.dataset.name, value: btn.dataset.next });
      if (r && r.ok === false) alert(`denied: ${r.error}`);
      refreshAll();
    });
    grid.appendChild(div);
  }
}

/* ---------- doctor ---------- */

function renderDoctor(report) {
  const box = $("doctor-body");
  box.innerHTML = "";
  for (const c of report.checks || []) {
    const div = document.createElement("div");
    div.className = "check";
    div.innerHTML =
      `<span class="check-name">${esc(c.name)}</span>` +
      `<span class="check-detail">${esc(c.detail)}</span>` +
      `<span class="check-ms">${Number(c.latency_ms).toFixed(1)} ms</span>`;
    box.appendChild(div);
  }
}

async function refreshOnce() {
  try {
    const d = await getJSON("/api/v1/doctor");
    renderDoctor(d);
  } catch { /* daemon down: status chip already reports it */ }
}

/* ---------- client review ---------- */

function renderReview(rows) {
  const body = document.getElementById("review-body");
  if (!body) return;
  body.textContent = "";
  const live = (rows || []).filter((r) => r.title !== null && r.title !== undefined);
  if (!live.length) {
    const empty = document.createElement("div");
    empty.className = "muted-note";
    const cmd = document.createElement("code");
    cmd.textContent = "cairn review publish --media cuts/v1.mp4";
    empty.append(document.createTextNode(t("empty.review").split(":")[0] + ": "), cmd);
    body.appendChild(empty);
    return;
  }
  for (const r of live) {
    const card = document.createElement("div");
    card.className = "review-project";

    const head = document.createElement("div");
    head.className = "review-head";
    const title = document.createElement("span");
    title.className = "rv-title";
    title.textContent = r.title;
    const links = document.createElement("span");
    links.className = "rv-links";
    links.textContent =
      r.live_links + " live link" + (r.live_links === 1 ? "" : "s") +
      (r.expired_links ? " · " + r.expired_links + " expired" : "");
    head.append(title, links);
    card.appendChild(head);

    const versions = document.createElement("div");
    versions.className = "rv-versions";
    const ordered = r.versions.slice(-4).reverse();
    ordered.forEach((v, i) => {
      const row = document.createElement("div");
      row.className = "rv-row" + (i === 0 ? " current" : "");
      const chip = document.createElement("span");
      chip.className = "rv-v";
      chip.textContent = "v" + v.number;
      const meta = document.createElement("span");
      meta.textContent = `${v.label} · ${v.duration} · ${v.frames} fr · by ${v.published_by}`;
      row.append(chip, meta);
      if (v.has_proxy) {
        const px = document.createElement("span");
        px.className = "rv-proxy";
        px.textContent = "proxy";
        row.append(px);
      }
      versions.appendChild(row);
    });
    card.appendChild(versions);

    const notes = document.createElement("div");
    notes.className = "rv-notes";
    notes.textContent = r.open_notes + " open note" + (r.open_notes === 1 ? "" : "s");
    card.appendChild(notes);
    body.appendChild(card);
  }
}

async function refreshReview() {
  try {
    const r = await getJSON("/api/v1/review");
    LAST_REVIEW = r.review || [];
    renderReview(LAST_REVIEW);
  } catch { /* dashboard keeps polling */ }
}

/* ---------- help overlay (audit #12) ---------- */

function toggleHelp(force) {
  const ov = $("help-overlay");
  ov.hidden = force !== undefined ? !force : !ov.hidden;
}
$("help-close").addEventListener("click", () => toggleHelp(false));
$("foot-help").addEventListener("click", () => toggleHelp(true));

/* ---------- keyboard: / search, ? help, g-then-x navigation ---------- */

let gPending = false;
document.addEventListener("keydown", (ev) => {
  const tag = (ev.target && ev.target.tagName) || "";
  const typing = tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
  if (ev.key === "Escape") {
    $("help-overlay").hidden = true;
    $("search-drop").hidden = true;
    return;
  }
  if (typing) return;
  if (ev.key === "/") {
    ev.preventDefault();
    $("search-input").focus();
    $("search-input").select();
    return;
  }
  if (ev.key === "?" || (ev.shiftKey && ev.key === "/")) {
    ev.preventDefault();
    toggleHelp();
    return;
  }
  if (ev.key === "g") { gPending = true; window.setTimeout(() => { gPending = false; }, 900); return; }
  if (gPending) {
    const map = { o: "#overview", f: "#files", p: "#projects", r: "#review", t: "#team", l: "#locks", v: "#versions", s: "#storage", d: "#doctor" };
    const href = map[ev.key.toLowerCase()];
    if (href) {
      ev.preventDefault();
      const el = document.querySelector(href);
      if (el) el.scrollIntoView({ behavior: "smooth", block: "start" });
      navActivate(href);
    }
    gPending = false;
  }
});

/* ---------- copy buttons ---------- */

function copyBtn(ev) {
  const text = ev.currentTarget.dataset.copy || "";
  navigator.clipboard
    .writeText(text)
    .then(() => {
      const btn = ev.currentTarget;
      const prev = btn.textContent;
      btn.textContent = "copied";
      window.setTimeout(() => { btn.textContent = prev; }, 1200);
    })
    .catch(() => {});
}
$("ob-cli-copy").addEventListener("click", copyBtn);

/* ---------- live presence (round 20, ADR-0023 §2) ----------
   SSE stream of editor playheads when this device opted in; the honest
   "presence off" chip otherwise (never a fake empty roster). */

let LIVE_SSE = null;
let LIVE_ROWS = new Map(); // from -> {project, editor, frame, rate, action, at}

function liveRow(ev) {
  let payload = {};
  try { payload = JSON.parse(ev.payload || "{}"); } catch { /* foreign schema — show raw */ }
  return {
    from: ev.from || "?",
    project: ev.project || "",
    editor: payload.editor || "",
    frame: Number.isFinite(payload.frame) ? payload.frame : null,
    rate: Number.isFinite(payload.rate) ? payload.rate : null,
    action: payload.action || "",
    local: ev.local === true,
    at: Date.now(),
  };
}

function renderLive() {
  const body = $("live-body");
  body.textContent = "";
  if (!LIVE_ROWS.size) {
    const empty = document.createElement("div");
    empty.className = "muted-note";
    empty.textContent = t("empty.live");
    body.appendChild(empty);
    return;
  }
  const list = document.createElement("ul");
  list.className = "audit-list";
  const rows = [...LIVE_ROWS.values()].sort((a, b) => (a.local === b.local ? a.editor.localeCompare(b.editor) : a.local ? -1 : 1));
  for (const r of rows) {
    const li = document.createElement("li");
    const tc = r.frame !== null && r.rate
      ? `${Math.floor(r.frame / (r.rate * 3600))}:${String(Math.floor((r.frame / (r.rate * 60)) % 60)).padStart(2, "0")}:${String(Math.floor((r.frame / r.rate) % 60)).padStart(2, "0")}:${String(Math.floor(r.frame % r.rate)).padStart(2, "0")}`
      : "—";
    li.innerHTML =
      `<span class="dot ${r.local ? "ok" : ""}"></span>` +
      `<span class="sans"><b>${esc(r.editor || r.from)}</b>${r.local ? " (you)" : ""}</span>` +
      `<span class="mono">${esc(tc)}</span>` +
      `<span class="dim">${esc(r.action || "")}</span>` +
      `<span class="mono dim">${esc(r.project)}</span>`;
    list.appendChild(li);
  }
  body.appendChild(list);
}

function liveSseOpen() {
  if (LIVE_SSE) return;
  try {
    LIVE_SSE = new EventSource("/api/v1/live");
    LIVE_SSE.onmessage = (msg) => {
      try {
        const ev = JSON.parse(msg.data);
        LIVE_ROWS.set(ev.from, liveRow(ev));
        // 15s staleness prune (matches the daemon-side TTL)
        for (const [k, r] of LIVE_ROWS) if (Date.now() - r.at > 15000) LIVE_ROWS.delete(k);
        renderLive();
      } catch { /* skip malformed event */ }
    };
    LIVE_SSE.onerror = () => { /* stream closed (flag flip / daemon down): next refresh re-opens */ };
  } catch { /* EventSource unavailable — snapshot polling still covers */ }
}

async function refreshLive() {
  try {
    const snap = await getJSON("/api/v1/live/snapshot");
    if (snap.enabled !== true) {
      if (LIVE_SSE) { LIVE_SSE.close(); LIVE_SSE = null; }
      LIVE_ROWS.clear();
      const note = $("live-note");
      if (note) note.textContent = t("live.off");
      const body = $("live-body");
      body.textContent = "";
      const chip = document.createElement("div");
      chip.className = "muted-note";
      chip.textContent = t("live.off");
      body.appendChild(chip);
      return;
    }
    const note = $("live-note");
    if (note) note.textContent = t("note.live");
    for (const p of snap.projects || []) {
      for (const ev of p.events || []) {
        LIVE_ROWS.set(ev.from, liveRow({ ...ev, project: p.project, local: false }));
      }
    }
    liveSseOpen();
    renderLive();
  } catch { /* daemon gone — status chip covers */ }
}

/* ---------- orchestration ---------- */

async function refreshAll() {
  await refreshStatus();
  await refreshProjects();
  renderOnboarding();
  await refreshStorage();
  await refreshFiles();
  try {
    const feed = await getJSON("/api/v1/feed");
    LAST_ACTIVITY = feed.activity || [];
    LAST_LOCKS = feed.leases || [];
    renderActivity(LAST_ACTIVITY);
    renderLocks(LAST_LOCKS);
  } catch { /* covered by status chip */ }
  await refreshSnapshots();
  await refreshPins();
  await pollRecallJobs();
  try {
    const f = await getJSON("/api/v1/flags");
    renderFlags(f.flags);
  } catch { /* covered */ }
  refreshLive();
}

let LAST_FILES = null;
let FILES_KEY = null;   // project+filter the current rows describe
let LAST_REVIEW = [];
let LAST_LOCKS = [];
let LAST_ACTIVITY = [];

/* ---------- action buttons ---------- */

$("btn-attach").addEventListener("click", async () => {
  const root = $("attach-root").value.trim();
  const project = $("attach-project").value.trim();
  if (!root) return alert("root path required");
  const r = await postJSON("/api/v1/attach", { root_path: root, project_id: project });
  if (!r.ok) alert(`attach failed: ${r.error}`);
  else { $("attach-root").value = ""; $("attach-project").value = ""; }
  refreshAll();
});

$("ob-cta-attach").addEventListener("click", () => {
  // browsers cannot hand a loopback page the absolute path of a picked
  // folder — focus the input and show the CLI copy (honest, no fake picker)
  $("attach-root").focus();
  $("attach-root").scrollIntoView({ behavior: "smooth", block: "center" });
});

$("files-filter").addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(refreshFiles, 220);
});

$("files-project").addEventListener("change", refreshFiles);
$("snapshot-project").addEventListener("change", refreshSnapshots);
$("pin-project").addEventListener("change", refreshPins);

$("btn-snapshot").addEventListener("click", async () => {
  const project = selectedProject("snapshot-project");
  if (!project) return alert("attach a project first");
  const r = await postJSON("/api/v1/snapshots", {
    project_id: project,
    label: $("snapshot-label").value.trim(),
  });
  if (r.ok) { $("snapshot-label").value = ""; refreshSnapshots(); }
  else alert(`version failed: ${r.error}`);
});

$("btn-snapshots-refresh").addEventListener("click", refreshSnapshots);

$("btn-pin").addEventListener("click", async () => {
  const project = selectedProject("pin-project");
  const path = $("pin-path").value.trim();
  if (!project || !path) return alert("project and path required");
  const r = await postJSON("/api/v1/pins", { project_id: project, path });
  if (r.ok) { $("pin-path").value = ""; refreshPins(); }
  else alert(`pin failed: ${r.error}`);
});

$("btn-recall").addEventListener("click", async () => {
  const project = selectedProject("recall-project");
  if (!project) return alert("attach a project first");
  const r = await postJSON("/api/v1/recall", {
    project_id: project,
    path: $("recall-path").value.trim(),
  });
  if (r.ok) { RECALL_JOBS.set(r.job_id, { state: "running", progress: 0 }); renderRecallJobs(); }
  else alert(`recall failed: ${r.error}`);
});

/* staggered card entry (taste-skill: cascade, never all at once) */
document.querySelectorAll(".card").forEach((el, i) => {
  el.style.setProperty("--i", String(i % 7));
});

/* nav active state */
document.querySelectorAll(".nav-item").forEach((a) => {
  a.addEventListener("click", () => navActivate(a.getAttribute("href")));
});

/* scroll-spy: the long console page makes anchor nav useless without it
   (design review: "nav items failed because I have to scroll through
   Team to get to Locks") — the active item now follows the viewport */
const SPY = new IntersectionObserver(
  (entries) => {
    for (const e of entries) {
      if (e.isIntersecting) navActivate("#" + e.target.id);
    }
  },
  { rootMargin: "-15% 0px -70% 0px" }
);
document.querySelectorAll(".main section[id]").forEach((s) => SPY.observe(s));

/* boot */
applyI18n();
refreshOnce();
refreshAll();
refreshReview();
refreshTeam();
refreshUpdate();
setInterval(refreshAll, 2000);
setInterval(refreshReview, 5000);
setInterval(refreshTeam, 8000);
setInterval(refreshUpdate, 30000);
setInterval(refreshOnce, 15000);
