/* cairn local console - the three-destination redesign.
   No build step, no framework, no fake data: empty states stay empty
   until real data exists, every server string is escaped before it
   touches innerHTML, and one state derivation feeds every dot on
   screen (topbar chip, rail, footer - they can never disagree).

   Views: Dashboard (overview), Files (focused workspace), Settings
   (all technical drawers). Slide-over panels carry the workflows
   (add / versions / recall). Poll cadence unchanged: 2s core, 5s
   review, 8s team, 15s doctor, 30s update. */

"use strict";

const $ = (id) => document.getElementById(id);

/* ============================== i18n ==============================
   Four languages, zero em-dashes (taste-skill hard ban: the dash
   character is forbidden in visible copy; "·" and "-" carry). */

const STR = {
  "a11y.skip": { en: "skip to content", "de-DE": "zum Inhalt springen", "ja-JP": "本文へスキップ", "zh-CN": "跳到正文" },

  "nav.dashboard": { en: "Dashboard", "de-DE": "Übersicht", "ja-JP": "ダッシュボード", "zh-CN": "总览" },
  "nav.files": { en: "Files", "de-DE": "Dateien", "ja-JP": "ファイル", "zh-CN": "文件" },
  "nav.settings": { en: "Settings", "de-DE": "Einstellungen", "ja-JP": "設定", "zh-CN": "设置" },

  "head.noRoots": { en: "no project", "de-DE": "kein Projekt", "ja-JP": "プロジェクトなし", "zh-CN": "无项目" },
  "head.roots": { en: "{n} projects", "de-DE": "{n} Projekte", "ja-JP": "{n}プロジェクト", "zh-CN": "{n} 个项目" },
  "search.placeholder": { en: "search files, projects, reviews", "de-DE": "Dateien, Projekte, Reviews suchen", "ja-JP": "ファイル・プロジェクト・レビューを検索", "zh-CN": "搜索文件、项目、审阅" },

  "ob.title": { en: "Welcome to Cairn", "de-DE": "Willkommen bei Cairn", "ja-JP": "Cairn へようこそ", "zh-CN": "欢迎使用 Cairn" },
  "ob.sub": { en: "Choose a project folder to attach.", "de-DE": "Wähle einen Projektordner zum Verbinden.", "ja-JP": "接続するプロジェクトフォルダーを選んでください。", "zh-CN": "选择要连接的项目文件夹。" },
  "ob.continue": { en: "Continue", "de-DE": "Weiter", "ja-JP": "続ける", "zh-CN": "继续" },
  "ob.daemonDown": { en: "Cannot reach the daemon", "de-DE": "Daemon nicht erreichbar", "ja-JP": "デーモンに接続できません", "zh-CN": "无法连接守护进程" },
  "ob.daemonDownSub": { en: "Start it with cairn daemon, then reload this page.", "de-DE": "Starte ihn mit cairn daemon und lade diese Seite neu.", "ja-JP": "cairn daemon で起動し、このページを再読み込みしてください。", "zh-CN": "请先运行 cairn daemon，然后刷新此页面。" },

  "btn.add": { en: "Add project", "de-DE": "Projekt hinzufügen", "ja-JP": "プロジェクト追加", "zh-CN": "添加项目" },
  "btn.attach": { en: "attach", "de-DE": "verbinden", "ja-JP": "接続", "zh-CN": "连接" },
  "btn.copy": { en: "copy", "de-DE": "kopieren", "ja-JP": "コピー", "zh-CN": "复制" },
  "btn.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "btn.recall": { en: "Recall", "de-DE": "Abrufen", "ja-JP": "リコール", "zh-CN": "取回" },
  "btn.snapshot": { en: "create version", "de-DE": "Version erstellen", "ja-JP": "バージョン作成", "zh-CN": "创建版本" },
  "btn.detach": { en: "detach", "de-DE": "trennen", "ja-JP": "切断", "zh-CN": "断开" },
  "btn.restore": { en: "restore", "de-DE": "wiederherstellen", "ja-JP": "復元", "zh-CN": "还原" },
  "btn.pin": { en: "pin", "de-DE": "anheften", "ja-JP": "ピン留め", "zh-CN": "置顶" },
  "btn.unpin": { en: "unpin", "de-DE": "Pin löschen", "ja-JP": "ピン解除", "zh-CN": "取消置顶" },

  "panel.add": { en: "Add a project", "de-DE": "Projekt hinzufügen", "ja-JP": "プロジェクトを追加", "zh-CN": "添加项目" },
  "panel.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "panel.recall": { en: "Recall", "de-DE": "Abrufen", "ja-JP": "リコール", "zh-CN": "取回" },
  "field.root": { en: "Project folder", "de-DE": "Projektordner", "ja-JP": "プロジェクトフォルダー", "zh-CN": "项目文件夹" },
  "field.project": { en: "Project id", "de-DE": "Projekt-ID", "ja-JP": "プロジェクトID", "zh-CN": "项目 ID" },
  "field.optional": { en: "optional", "de-DE": "optional", "ja-JP": "任意", "zh-CN": "可选" },
  "or.cli": { en: "or from the terminal", "de-DE": "oder aus dem Terminal", "ja-JP": "またはターミナルから", "zh-CN": "或在终端执行" },

  "attach.root": { en: "/path/to/workspace", "de-DE": "/pfad/zum/workspace", "ja-JP": "/path/to/workspace", "zh-CN": "/path/to/workspace" },
  "attach.project": { en: "project id (optional)", "de-DE": "Projekt-ID (optional)", "ja-JP": "プロジェクトID（任意）", "zh-CN": "项目 ID（可选）" },
  "versions.label": { en: "label (e.g. before color pass)", "de-DE": "Label (z. B. vor dem Farbdurchgang)", "ja-JP": "ラベル（例: カラー前）", "zh-CN": "标签（如：调色前）" },
  "recall.path": { en: "one path (optional, whole project when empty)", "de-DE": "ein Pfad (optional, sonst ganzes Projekt)", "ja-JP": "1パス（任意、空なら全プロジェクト）", "zh-CN": "单个路径（可选，留空则整个项目）" },

  "card.assets": { en: "Pinned assets", "de-DE": "Angeheftete Dateien", "ja-JP": "ピン留めアセット", "zh-CN": "置顶资产" },
  "card.sessions": { en: "Sessions & activity", "de-DE": "Sitzungen & Aktivität", "ja-JP": "セッションとアクティビティ", "zh-CN": "会话与动态" },
  "note.assets": { en: "Pinned files stay on this machine and never evict.", "de-DE": "Angeheftete Dateien bleiben auf dieser Maschine und werden nie geräumt.", "ja-JP": "ピン留めファイルはこの端末に保持され、退避されません。", "zh-CN": "置顶文件保留在本机，永不被清理。" },
  "empty.assets": { en: "Nothing pinned yet - pin files you always need locally.", "de-DE": "Noch nichts angeheftet - pinne Dateien, die du immer lokal brauchst.", "ja-JP": "まだピン留めなし - 常にローカルで必要なファイルをピン留めしてください。", "zh-CN": "还没有置顶 - 把总要在本地用的文件置顶吧。" },
  "label.sessions": { en: "sessions", "de-DE": "Sitzungen", "ja-JP": "セッション", "zh-CN": "会话" },
  "label.journal": { en: "journal", "de-DE": "Journal", "ja-JP": "ジャーナル", "zh-CN": "日志" },
  "act.saved": { en: "saved", "de-DE": "gespeichert", "ja-JP": "保存", "zh-CN": "保存" },
  "act.renamed": { en: "renamed", "de-DE": "umbenannt", "ja-JP": "改名", "zh-CN": "重命名" },
  "act.deleted": { en: "deleted", "de-DE": "gelöscht", "ja-JP": "削除", "zh-CN": "删除" },

  "spark.note": { en: "journal volume · last {n} saves", "de-DE": "Journal-Volumen · letzte {n} Speicherungen", "ja-JP": "ジャーナル量 · 直近{n}保存", "zh-CN": "日志量 · 最近 {n} 次保存" },
  "presence.live": { en: "live · {n} editing", "de-DE": "live · {n} am Schneiden", "ja-JP": "ライブ · {n}人が編集中", "zh-CN": "实时 · {n} 人在剪" },
  "review.label": { en: "Client review", "de-DE": "Kunden-Review", "ja-JP": "クライアントレビュー", "zh-CN": "客户审阅" },
  "review.notes": { en: "{n} open notes", "de-DE": "{n} offene Notizen", "ja-JP": "未解決ノート{n}件", "zh-CN": "{n} 条待处理批注" },

  "set.system": { en: "System", "de-DE": "System", "ja-JP": "システム", "zh-CN": "系统" },
  "set.daemon": { en: "daemon", "de-DE": "Daemon", "ja-JP": "デーモン", "zh-CN": "守护进程" },
  "set.proto": { en: "protocol", "de-DE": "Protokoll", "ja-JP": "プロトコル", "zh-CN": "协议" },
  "set.uptime": { en: "uptime", "de-DE": "Laufzeit", "ja-JP": "稼働時間", "zh-CN": "运行时长" },
  "set.node": { en: "node", "de-DE": "Knoten", "ja-JP": "ノード", "zh-CN": "节点" },
  "set.update": { en: "update", "de-DE": "Update", "ja-JP": "アップデート", "zh-CN": "更新" },
  "set.updateOk": { en: "up to date", "de-DE": "aktuell", "ja-JP": "最新です", "zh-CN": "已是最新" },
  "set.doctor": { en: "System health (doctor)", "de-DE": "Systemzustand (Doctor)", "ja-JP": "システム健全性（ドクター）", "zh-CN": "系统健康（诊断）" },
  "set.flags": { en: "Feature flags", "de-DE": "Funktions-Schalter", "ja-JP": "機能フラグ", "zh-CN": "功能开关" },
  "set.storage": { en: "Storage", "de-DE": "Speicher", "ja-JP": "ストレージ", "zh-CN": "存储" },
  "set.paths": { en: "Projects & paths", "de-DE": "Projekte & Pfade", "ja-JP": "プロジェクトとパス", "zh-CN": "项目与路径" },
  "set.appearance": { en: "Appearance", "de-DE": "Darstellung", "ja-JP": "外観", "zh-CN": "外观" },
  "set.quota": { en: "disk quota", "de-DE": "Datenträger-Kontingent", "ja-JP": "ディスク容量", "zh-CN": "磁盘配额" },
  "set.themeLight": { en: "Light", "de-DE": "Hell", "ja-JP": "ライト", "zh-CN": "浅色" },
  "set.themeDark": { en: "Dark", "de-DE": "Dunkel", "ja-JP": "ダーク", "zh-CN": "深色" },
  "set.themeSystem": { en: "System", "de-DE": "System", "ja-JP": "システム", "zh-CN": "跟随系统" },
  "note.theme": { en: "Dark mode follows your choice here; the top bar toggle is the shortcut.", "de-DE": "Der Dunkelmodus folgt dieser Wahl; der Schalter oben ist die Abkürzung.", "ja-JP": "ダークモードはここでの選択に従います。上部の切替がショートカットです。", "zh-CN": "深色模式以此处选择为准；顶栏按钮是快捷方式。" },

  "stat.chunks": { en: "local chunks", "de-DE": "lokale Chunks", "ja-JP": "ローカルチャンク", "zh-CN": "本地块" },
  "stat.cached": { en: "cached", "de-DE": "gecacht", "ja-JP": "キャッシュ量", "zh-CN": "已缓存" },
  "stat.pinned": { en: "pinned chunks", "de-DE": "angeheftete Chunks", "ja-JP": "ピン留めチャンク", "zh-CN": "置顶块" },

  "note.flags": { en: "Flags apply on the next job run, no restart. Owner and lead only (RBAC enforced).", "de-DE": "Schalter greifen beim nächsten Job-Lauf, ohne Neustart. Nur Owner und Lead (RBAC).", "ja-JP": "フラグは次のジョブ実行時に反映。再起動不要。オーナーとリードのみ（RBAC強制）。", "zh-CN": "开关在下次任务运行时生效，无需重启。仅所有者与主管（RBAC 强制）。" },
  "note.locks": { en: "NLE project files lock automatically on open; fencing tokens protect every append.", "de-DE": "NLE-Projektdateien sperren beim Öffnen automatisch; Fencing-Token schützen jeden Anhang.", "ja-JP": "NLEプロジェクトファイルはオープン時に自動ロック。フェンシングトークンが追記を保護します。", "zh-CN": "NLE 工程文件打开时自动加锁；栅栏令牌保护每次追加。" },
  "note.versions": { en: "Versions fold from the journal every 5,000 entries, every 24h, on demand, or at project close. Every fold is a restore point.", "de-DE": "Versionen falten aus dem Journal alle 5.000 Einträge, alle 24h, auf Abruf oder bei Projektende. Jede Faltung ist ein Wiederherstellungspunkt.", "ja-JP": "バージョンは5,000エントリごと・24時間ごと・要求時・プロジェクト終了時にジャーナルから折りたたまれます。各折返しが復元点です。", "zh-CN": "版本按每 5,000 条、每 24 小时、按需或工程结束时从日志折叠生成。每次折叠都是一个还原点。" },
  "note.recall": { en: "Recall materializes files from the bucket into local storage. Whole project when the path is empty.", "de-DE": "Abruf materialisiert Dateien aus dem Bucket in den lokalen Speicher. Leerer Pfad = ganzes Projekt.", "ja-JP": "リコールはバケットからファイルをローカルへ実体化します。パスが空なら全プロジェクト。", "zh-CN": "取回将文件从桶实体化到本地。路径留空则取回整个项目。" },
  "note.storage": { en: "NLE media caches belong on local scratch; pinned chunks are never evicted.", "de-DE": "NLE-Medien-Caches gehören auf lokalen Scratch; angeheftete Chunks werden nie geräumt.", "ja-JP": "NLEメディアキャッシュはローカルスクラッチに。ピン留めチャンクは退避されません。", "zh-CN": "NLE 媒体缓存应放在本地暂存盘；置顶块永不被淘汰。" },
  "note.live": { en: "Live presence streams other editors' playheads. Opt in with the live_presence flag.", "de-DE": "Live-Präsenz streamt die Playheads anderer Editoren. Aktiviere das Flag live_presence.", "ja-JP": "ライブプレゼンスは他の編集者のプレイヘッドを流します。live_presence フラグで有効化。", "zh-CN": "实时在线会显示其他剪辑师的播放头。通过 live_presence 标志开启。" },
  "live.off": { en: "Live presence is off on this device - flip the live_presence flag (applies at next swarm join).", "de-DE": "Live-Präsenz ist auf diesem Gerät aus - live_presence umschalten (greift beim nächsten Swarm-Join).", "ja-JP": "この端末ではライブプレゼンスはオフ - live_presence を切替（次回の swarm 参加時に有効）。", "zh-CN": "本设备的实时在线已关闭 - 切换 live_presence 标志（下次加入 swarm 时生效）。" },

  "files.filter": { en: "search this project", "de-DE": "dieses Projekt durchsuchen", "ja-JP": "このプロジェクト内を検索", "zh-CN": "在项目内搜索" },
  "files.summary": { en: "{files} files · {synced} synced · {syncing} syncing · {conflict} conflict", "de-DE": "{files} Dateien · {synced} synchron · {syncing} läuft · {conflict} Konflikt", "ja-JP": "{files} ファイル · 同期済み {synced} · 同期中 {syncing} · 競合 {conflict}", "zh-CN": "{files} 个文件 · 已同步 {synced} · 同中 {syncing} · 冲突 {conflict}" },
  "files.synced": { en: "synced", "de-DE": "synchron", "ja-JP": "同期済み", "zh-CN": "已同步" },
  "files.syncing": { en: "syncing", "de-DE": "läuft", "ja-JP": "同期中", "zh-CN": "同步中" },
  "files.conflict": { en: "conflict", "de-DE": "Konflikt", "ja-JP": "競合", "zh-CN": "冲突" },
  "files.placeholder": { en: "placeholder", "de-DE": "Platzhalter", "ja-JP": "プレースホルダー", "zh-CN": "占位" },
  "files.pinnedA11y": { en: "pinned", "de-DE": "angepinnt", "ja-JP": "ピン留め済み", "zh-CN": "已置顶" },
  "files.error": { en: "couldn't reach the daemon - the list fills in the moment it responds", "de-DE": "Daemon nicht erreichbar - die Liste erscheint, sobald er antwortet", "ja-JP": "デーモンに接続できません - 応答すると一覧が表示されます", "zh-CN": "无法连接守护进程 - 响应后列表会自动出现" },

  "th.file": { en: "file", "de-DE": "Datei", "ja-JP": "ファイル", "zh-CN": "文件" },
  "th.size": { en: "size", "de-DE": "Größe", "ja-JP": "サイズ", "zh-CN": "大小" },
  "th.sync": { en: "sync", "de-DE": "Sync", "ja-JP": "同期", "zh-CN": "同步" },
  "th.actions": { en: "actions", "de-DE": "Aktionen", "ja-JP": "操作", "zh-CN": "操作" },
  "th.label": { en: "label", "de-DE": "Label", "ja-JP": "ラベル", "zh-CN": "标签" },
  "th.author": { en: "author", "de-DE": "Autor", "ja-JP": "作成者", "zh-CN": "作者" },
  "th.member": { en: "member", "de-DE": "Mitglied", "ja-JP": "メンバー", "zh-CN": "成员" },
  "th.role": { en: "role", "de-DE": "Rolle", "ja-JP": "役割", "zh-CN": "角色" },

  "empty.projects": { en: "No attached projects - add one with the button above.", "de-DE": "Keine verbundenen Projekte - oben hinzufügen.", "ja-JP": "接続済みプロジェクトはありません - 上のボタンで追加。", "zh-CN": "暂无已连接项目 - 用上方按钮添加。" },
  "empty.files": { en: "No files yet - attach a project and drop media in.", "de-DE": "Noch keine Dateien - Projekt verbinden und Medien ablegen.", "ja-JP": "ファイルはまだありません - プロジェクトを接続してメディアを置いてください。", "zh-CN": "暂无文件 - 连接项目后放入媒体。" },
  "empty.activity": { en: "No entries yet - saves appear here as they are journaled.", "de-DE": "Noch keine Einträge - Speicherungen erscheinen hier, sobald sie protokolliert sind.", "ja-JP": "エントリはまだありません - 保存が記録されるとここに表示されます。", "zh-CN": "暂无记录 - 保存入日志后会显示在这里。" },
  "empty.locks": { en: "No live locks on this machine.", "de-DE": "Keine aktiven Sperren auf dieser Maschine.", "ja-JP": "このマシンにアクティブなロックはありません。", "zh-CN": "本机暂无活动锁定。" },
  "empty.versions": { en: "No versions yet - create one after the first sync.", "de-DE": "Noch keine Versionen - nach dem ersten Sync eine erstellen.", "ja-JP": "バージョンはまだありません - 初回同期後に作成してください。", "zh-CN": "暂无版本 - 首次同步后创建一个。" },
  "empty.recall": { en: "No recall jobs yet.", "de-DE": "Noch keine Abruf-Jobs.", "ja-JP": "リコールジョブはまだありません。", "zh-CN": "暂无取回任务。" },
  "empty.team": { en: "No project attached - the team roster syncs with the project.", "de-DE": "Kein Projekt verbunden - die Teamliste synchronisiert mit dem Projekt.", "ja-JP": "プロジェクトが未接続 - チーム名簿はプロジェクトと同期されます。", "zh-CN": "未连接项目 - 团队名册随项目同步。" },
  "empty.spark": { en: "journal volume", "de-DE": "Journal-Volumen", "ja-JP": "ジャーナル量", "zh-CN": "日志量" },

  "team.myRole": { en: "your role", "de-DE": "deine Rolle", "ja-JP": "あなたの役割", "zh-CN": "你的角色" },
  "team.invite": { en: "invite a machine - join code", "de-DE": "Maschine einladen - Beitrittscode", "ja-JP": "マシンを招待 - 参加コード", "zh-CN": "邀请设备 - 加入码" },
  "team.audit": { en: "Recent decisions (synced audit ledger)", "de-DE": "Letzte Entscheidungen (synced Audit-Log)", "ja-JP": "最近の判断（同期される監査台帳）", "zh-CN": "最近的权限判定（随项目同步的审计账）" },
  "team.allowed": { en: "allowed", "de-DE": "erlaubt", "ja-JP": "許可", "zh-CN": "允许" },
  "team.denied": { en: "denied", "de-DE": "verweigert", "ja-JP": "拒否", "zh-CN": "拒绝" },
  "team.you": { en: "you", "de-DE": "du", "ja-JP": "あなた", "zh-CN": "你" },

  "chip.ok": { en: "all synced", "de-DE": "alles synchron", "ja-JP": "すべて同期済み", "zh-CN": "全部已同步" },
  "chip.warn": { en: "syncing", "de-DE": "synchronisiert", "ja-JP": "同期中", "zh-CN": "同步中" },
  "chip.bad": { en: "daemon unreachable", "de-DE": "Daemon nicht erreichbar", "ja-JP": "デーモン到達不可", "zh-CN": "守护进程不可达" },
  "chip.update": { en: "update available", "de-DE": "Update verfügbar", "ja-JP": "アップデートあり", "zh-CN": "有可用更新" },
  "chip.updateFailed": { en: "update check failed", "de-DE": "Update-Prüfung fehlgeschlagen", "ja-JP": "アップデート確認に失敗", "zh-CN": "更新检查失败" },
  "node.online": { en: "online", "de-DE": "online", "ja-JP": "オンライン", "zh-CN": "在线" },
  "node.offline": { en: "offline", "de-DE": "offline", "ja-JP": "オフライン", "zh-CN": "离线" },

  "quota.warn": { en: "store volume is above 95% - archive or evict before sync stalls", "de-DE": "Speichervolumen über 95% - archivieren oder räumen, bevor der Sync stockt", "ja-JP": "ストア領域が95%超 - 同期が止まる前に整理を", "zh-CN": "存储卷已超 95% - 请归档或清理，避免同步停滞" },

  "help.title": { en: "Shortcuts & cheatsheet", "de-DE": "Kürzel & Spickzettel", "ja-JP": "ショートカットとチートシート", "zh-CN": "快捷键与速查表" },
  "help.keys": { en: "Keys", "de-DE": "Tasten", "ja-JP": "キー", "zh-CN": "按键" },
  "help.cli": { en: "CLI cheatsheet", "de-DE": "CLI-Spickzettel", "ja-JP": "CLIチートシート", "zh-CN": "命令速查" },
  "help.states": { en: "What the dot means", "de-DE": "Was der Punkt bedeutet", "ja-JP": "ドットの意味", "zh-CN": "状态点含义" },
  "help.search": { en: "focus search", "de-DE": "Suche fokussieren", "ja-JP": "検索にフォーカス", "zh-CN": "聚焦搜索" },
  "help.help": { en: "toggle this panel", "de-DE": "dieses Panel umschalten", "ja-JP": "このパネルの切替", "zh-CN": "切换此面板" },
  "help.goDash": { en: "go to dashboard", "de-DE": "zur Übersicht", "ja-JP": "ダッシュボードへ", "zh-CN": "前往总览" },
  "help.goFiles": { en: "go to files", "de-DE": "zu den Dateien", "ja-JP": "ファイルへ", "zh-CN": "前往文件" },
  "help.goSettings": { en: "go to settings", "de-DE": "zu den Einstellungen", "ja-JP": "設定へ", "zh-CN": "前往设置" },
  "help.esc": { en: "close panels and overlays", "de-DE": "Panels und Overlays schließen", "ja-JP": "パネルとオーバーレイを閉じる", "zh-CN": "关闭面板与浮层" },
  "help.dotOk": { en: "all files synced, safe to edit", "de-DE": "alle Dateien synchron, sicher zum Schneiden", "ja-JP": "全ファイル同期済み、編集しても安全", "zh-CN": "全部已同步，可安全编辑" },
  "help.dotWarn": { en: "syncing, chunks in flight", "de-DE": "synchronisiert, Chunks unterwegs", "ja-JP": "同期中、チャンク転送中", "zh-CN": "同步中，数据块传输中" },
  "help.dotBad": { en: "attention, open Settings", "de-DE": "Achtung, Einstellungen öffnen", "ja-JP": "要注意、設定を確認", "zh-CN": "需要注意，请查看设置" },

  "toast.attached": { en: "project attached", "de-DE": "Projekt verbunden", "ja-JP": "プロジェクトを接続しました", "zh-CN": "项目已连接" },
  "toast.detached": { en: "project detached, local files stay", "de-DE": "Projekt getrennt, lokale Dateien bleiben", "ja-JP": "プロジェクトを切断、ローカルファイルは保持", "zh-CN": "项目已断开，本地文件保留" },
  "toast.pinned": { en: "pinned", "de-DE": "angepinnt", "ja-JP": "ピン留めしました", "zh-CN": "已置顶" },
  "toast.unpinned": { en: "unpinned", "de-DE": "Pin gelöst", "ja-JP": "ピン留めを解除", "zh-CN": "已取消置顶" },
  "toast.recallStarted": { en: "recall started", "de-DE": "Abruf gestartet", "ja-JP": "リコール開始", "zh-CN": "取回已开始" },
  "toast.versionCreated": { en: "version created", "de-DE": "Version erstellt", "ja-JP": "バージョン作成済み", "zh-CN": "版本已创建" },
  "toast.restored": { en: "restored {n} files ({b})", "de-DE": "{n} Dateien wiederhergestellt ({b})", "ja-JP": "{n}ファイルを復元（{b}）", "zh-CN": "已还原 {n} 个文件（{b}）" },
  "toast.copied": { en: "copied", "de-DE": "kopiert", "ja-JP": "コピーしました", "zh-CN": "已复制" },
  "toast.denied": { en: "denied: {e}", "de-DE": "verweigert: {e}", "ja-JP": "拒否: {e}", "zh-CN": "被拒绝：{e}" },
  "toast.failed": { en: "failed: {e}", "de-DE": "fehlgeschlagen: {e}", "ja-JP": "失敗: {e}", "zh-CN": "失败：{e}" },

  "confirm.detach": { en: "Detach {p}? Local files stay.", "de-DE": "{p} trennen? Lokale Dateien bleiben.", "ja-JP": "{p}を切断しますか？ローカルファイルは保持されます。", "zh-CN": "断开 {p}？本地文件会保留。" },
  "confirm.restore": { en: "Restore this version into the workspace?", "de-DE": "Diese Version in den Workspace zurückspielen?", "ja-JP": "このバージョンをワークスペースに復元しますか？", "zh-CN": "将该版本还原到工作区？" },
};

function detectLang() {
  const saved = localStorage.getItem("cairn-lang");
  if (saved && STR["nav.overview"] && STR["nav.overview"][saved]) return saved;
  if (saved && STR["nav.files"][saved]) return saved;
  const nav = (navigator.language || "en").trim();
  if (STR["nav.files"][nav]) return nav;
  const base = nav.split("-")[0];
  if (base === "de") return "de-DE";
  if (base === "ja") return "ja-JP";
  if (base === "zh") return "zh-CN";
  return "en";
}
let LANG = detectLang();

function t(key, vars) {
  const row = STR[key];
  let v = row ? (row[LANG] || row.en || key) : key;
  if (vars) for (const k of Object.keys(vars)) v = v.split(`{${k}}`).join(vars[k]);
  return v;
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
    rerenderAllDynamic();
  });
});

/* ============================== safety + format ============================== */

function esc(s) {
  return String(s ?? "")
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;")
    .replace(/'/g, "&#39;");
}

function fmtBytes(n) {
  if (!Number.isFinite(n)) return "-";
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
  if (h > 0) return `${h}h ${m % 60}m`;
  if (m > 0) return `${m}m ${s % 60}s`;
  return `${s}s`;
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

/* split "dir/base" (both separators) - the table shows the base in
   sans and the directory in quiet mono below it */
function splitPath(p) {
  const s = String(p ?? "");
  const i = Math.max(s.lastIndexOf("/"), s.lastIndexOf("\\"));
  return i < 0 ? ["", s] : [s.slice(0, i + 1), s.slice(i + 1)];
}

/* Windows extended-length paths display without the \\?\ noise;
   the full string is still what copy uses and what title shows */
function displayRoot(p) {
  return String(p ?? "").replace(/^\\\\\?\\/, "");
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

/* toast: inline feedback instead of window.alert (redesign-skill ban) */
let TOAST_TIMER = null;
function toast(msg, isBad) {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    el.setAttribute("role", "status");
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.classList.toggle("is-bad", !!isBad);
  el.classList.add("is-on");
  window.clearTimeout(TOAST_TIMER);
  TOAST_TIMER = window.setTimeout(() => el.classList.remove("is-on"), 2800);
}

/* staggered entry indexes (taste-skill: cascade, never all at once) */
function stagger(scope, sel, cap) {
  scope.querySelectorAll(sel).forEach((el, i) => {
    el.style.setProperty("--i", String(i % (cap || 8)));
  });
}

/* ============================== theme ============================== */

const THEME = {
  mode: null, // "light" | "dark" | "system"
  mq: window.matchMedia("(prefers-color-scheme: dark)"),

  resolved() {
    if (this.mode === "system") return this.mq.matches ? "dark" : "light";
    return this.mode || "light";
  },
  apply() {
    const dark = this.resolved() === "dark";
    document.documentElement.dataset.theme = dark ? "dark" : "light";
    const meta = document.querySelector('meta[name="theme-color"]');
    if (meta) meta.setAttribute("content", dark ? "#131312" : "#fbfbfa");
    document.querySelectorAll(".seg").forEach((s) => {
      s.classList.toggle("is-active", s.dataset.themeSet === this.mode);
    });
  },
  set(mode) {
    this.mode = mode;
    try { localStorage.setItem("cairn-theme", mode); } catch { /* private mode */ }
    this.apply();
  },
  init() {
    let saved = "system";
    try { saved = localStorage.getItem("cairn-theme") || "system"; } catch { /* ok */ }
    this.mode = ["light", "dark", "system"].includes(saved) ? saved : "system";
    this.mq.addEventListener("change", () => { if (this.mode === "system") this.apply(); });
    this.apply();
  },
};
THEME.init();

$("theme-toggle").addEventListener("click", () => {
  THEME.set(THEME.resolved() === "dark" ? "light" : "dark");
});
document.querySelectorAll(".seg").forEach((s) => {
  s.addEventListener("click", () => THEME.set(s.dataset.themeSet));
});

/* ============================== views ============================== */

const VIEWS = ["dashboard", "files", "settings"];
let ACTIVE_VIEW = "dashboard";

function showView(name, focus) {
  if (!VIEWS.includes(name)) name = "dashboard";
  ACTIVE_VIEW = name;
  for (const v of VIEWS) {
    const el = $(`view-${v}`);
    if (el) el.classList.toggle("is-active", v === name);
  }
  document.querySelectorAll(".rail-item").forEach((b) => {
    b.classList.toggle("is-active", b.dataset.view === name);
  });
  if (history.replaceState) history.replaceState(null, "", `#${name}`);
  if (focus) {
    const f = $(focus);
    if (f) { f.focus(); f.select && f.select(); }
  }
}

document.querySelectorAll(".rail-item").forEach((b) => {
  b.addEventListener("click", () => showView(b.dataset.view));
});

/* search targets remap to the three destinations */
const TARGET_MAP = {
  "#overview": "dashboard", "#projects": "settings", "#files": "files",
  "#activity": "dashboard", "#review": "dashboard", "#team": "settings",
  "#live": "dashboard", "#locks": "settings", "#versions": "files",
  "#pins": "files", "#recall": "files", "#storage": "settings",
  "#flags": "settings", "#doctor": "settings", "#dashboard": "dashboard",
  "#settings": "settings",
};

/* ============================== shared state ============================== */

let PROJECTS = [];
let HEALTHY = false;
let DAEMON_UP = null;
let VERSION_STR = "";

const FOOT = {
  pending: null, cursor: null, conflicts: null, files: null, synced: null,
  disk: null,
};
let LAST_FILES = null;
let FILES_KEY = null;
let LAST_REVIEW = [];
let LAST_LOCKS = [];
let LAST_ACTIVITY = [];
const PIN_ROWS = [];      // {project, project_name, path, size, state}
const RECALL_JOBS = new Map();

/* ONE derivation feeds the topbar chip, the rail and the footer dot
   (the old UI could show "all files synced" while a project errored;
   this cannot disagree with itself) */
function deriveState() {
  if (DAEMON_UP === false) return "bad";
  const hasError = PROJECTS.some((p) => p.state === "error");
  const inFlight =
    (FOOT.pending ?? 0) > 0 ||
    PROJECTS.some((p) => p.state === "syncing") ||
    (FOOT.files ?? 0) > (FOOT.synced ?? 0);
  if (!HEALTHY || (FOOT.conflicts ?? 0) > 0 || hasError) return inFlight ? "warn" : "bad";
  if (inFlight) return "warn";
  return "ok";
}

function stateLabel(st) {
  return st === "ok" ? t("chip.ok") : st === "warn" ? t("chip.warn") : t("chip.bad");
}

function paintState() {
  const st = deriveState();
  const chip = $("state-chip");
  chip.className = `state-chip is-${st}`;
  $("state-label").textContent = stateLabel(st);

  // the rail carries identity only (version) - the state lives in the
  // topbar chip and the footer dot; three zones showing the same word
  // was the redundancy the review caught
  $("rail-version").textContent = VERSION_STR ? `daemon ${VERSION_STR}` : "daemon";

  const sb = document.querySelector(".sb-state");
  if (sb) {
    sb.className = `sb-state is-${st}`;
    $("foot-dot").className = `dot dot-${st === "ok" ? "ok" : st === "warn" ? "warn" : "bad"}`;
    $("foot-state-label").textContent = stateLabel(st);
  }
  renderFooter();
}

/* ============================== footer ============================== */

function renderFooter() {
  if (FOOT.files !== null) {
    $("foot-sync-n").textContent = `${FOOT.synced ?? 0}/${FOOT.files}`;
    const total = Math.max(1, FOOT.files);
    const fill = $("foot-files-fill");
    fill.style.width = `${Math.max(2, Math.round(((FOOT.synced ?? 0) / total) * 100))}%`;
    fill.style.background = (FOOT.conflicts ?? 0) > 0 || PROJECTS.some((p) => p.state === "error")
      ? "#d9a13c" : "#46a352";
  }
  if (FOOT.disk && Number.isFinite(FOOT.disk.total)) {
    const used = Math.max(0, FOOT.disk.total - FOOT.disk.free);
    $("foot-quota-pill").textContent = `${fmtBytes(used)} / ${fmtBytes(FOOT.disk.total)}`;
  }
}

/* ============================== onboarding ============================== */

function renderOnboarding() {
  const stage = $("onboarding");
  if (!stage) return;
  const attached = PROJECTS.length > 0;

  stage.hidden = attached;
  $("app").hidden = !attached;
  $("foot-progress").hidden = attached;
  $("foot-status").hidden = !attached;
  $("ob-error").hidden = !(DAEMON_UP === false);

  if (attached) { paintState(); return; }

  $("ob-title").textContent = t("ob.title");
  $("ob-sub").textContent = t("ob.sub");
  $("ob-continue").textContent = t("ob.continue");

  // the stage only exists at zero roots, so the track honestly sits at 1/3
  $("foot-track-fill").style.width = `${(1 / 3) * 100}%`;
  $("foot-progress").setAttribute("aria-valuenow", "1");
  $("foot-stage-label").textContent = `1 / 3 · ${t("ob.continue")}`;
}

/* progressive disclosure: first Continue reveals the attach scene,
   then gets out of the way - the scene's own attach button takes over */
$("ob-continue").addEventListener("click", () => {
  const scene = $("ob-attach");
  if (scene.hidden) {
    scene.hidden = false;
    $("ob-continue").hidden = true;
    $("ob-root").focus();
  }
});

/* ============================== status + settings system ============================== */

async function refreshStatus() {
  try {
    const s = await getJSON("/api/v1/status");
    VERSION_STR = `v${s.version}`;
    $("set-daemon").textContent = `v${s.version}`;
    $("set-proto").textContent = `v${s.proto}`;
    $("set-uptime").textContent = fmtUptime(s.uptime_ms);

    const summary = s.summary || {};
    HEALTHY = summary.healthy === true;
    DAEMON_UP = true;

    FOOT.pending = summary.outbox_pending ?? 0;
    FOOT.cursor = summary.journal_cursor ?? 0;
    FOOT.conflicts = summary.conflicts ?? 0;
    if (FOOT.files === null) FOOT.files = summary.files ?? 0;

    // hydration I1: the number is real telemetry, its home is Settings
    const i1 = summary.hydration_first_byte_ms;
    const doctor = $("doctor-body");
    if (doctor && Number.isFinite(i1)) {
      let row = doctor.querySelector("[data-i1]");
      if (!row) {
        row = document.createElement("div");
        row.className = "check";
        row.setAttribute("data-i1", "");
        doctor.prepend(row);
      }
      row.innerHTML =
        `<span class="check-name">hydration.first_byte</span>` +
        `<span class="check-ms">${i1.toFixed(1)} ms</span>` +
        `<span class="check-detail">target &lt; 50 ms · header cache</span>`;
    }
    paintState();
  } catch {
    HEALTHY = false;
    DAEMON_UP = false;
    $("set-node").textContent = t("node.offline");
    paintState();
    renderOnboarding();
  }
}

/* ============================== storage ============================== */

async function refreshStorage() {
  try {
    const r = await getJSON("/api/v1/storage");
    if (r.ok !== true) return;
    const b = r.blobs || {};
    $("stat-blobs").textContent = String(b.count ?? 0);
    $("stat-bytes").textContent = fmtBytes(b.bytes ?? 0);
    $("stat-pinned").textContent = String(b.pinned_count ?? 0);
    const note = $("storage-note");

    if (r.disk && Number.isFinite(r.disk.free_bytes) && Number.isFinite(r.disk.total_bytes) && r.disk.total_bytes > 0) {
      const used = Math.max(0, r.disk.total_bytes - r.disk.free_bytes);
      FOOT.disk = { total: r.disk.total_bytes, free: r.disk.free_bytes };
      renderFooter();
      const pct = Math.min(100, (used / r.disk.total_bytes) * 100);
      const fill = $("quota-fill");
      fill.style.width = `${Math.max(2, Math.round(pct))}%`;
      fill.style.background = pct >= 95 ? "#d96b66" : "#46a352";
      $("set-quota").textContent = `${fmtBytes(used)} / ${fmtBytes(r.disk.total_bytes)}`;
      if (pct >= 95) note.textContent = t("quota.warn");
    }
  } catch { /* stats stay at their last honest value */ }
}

/* ============================== update ============================== */

async function refreshUpdate() {
  try {
    const r = await getJSON("/api/v1/update");
    const chip = $("update-chip");
    if (!chip || r.ok !== true) return;
    if (r.update_offered) {
      chip.hidden = false;
      chip.className = "state-chip chip-update is-warn";
      $("update-label").textContent = t("chip.update");
      $("set-update").textContent = t("chip.update");
    } else if (r.check_failed) {
      chip.hidden = false;
      chip.className = "state-chip chip-update is-bad";
      $("update-label").textContent = t("chip.updateFailed");
      $("set-update").textContent = t("chip.updateFailed");
    } else {
      chip.hidden = true;
      $("set-update").textContent = t("set.updateOk");
    }
  } catch { /* never a lie on failure: chip stays hidden */ }
}

/* ============================== projects + sessions + paths ============================== */

function fillProjectSelects() {
  const el = $("files-project");
  if (!el) return;
  const prev = el.value;
  el.innerHTML = "";
  for (const p of PROJECTS) {
    const opt = document.createElement("option");
    opt.value = p.project_id;
    opt.textContent = p.display_name ? `${p.display_name}` : p.project_id;
    el.appendChild(opt);
  }
  if (prev && PROJECTS.some((p) => p.project_id === prev)) el.value = prev;
}

function renderSessions() {
  const list = $("session-list");
  list.textContent = "";
  $("sessions-count").textContent = PROJECTS.length ? String(PROJECTS.length) : "";

  if (!PROJECTS.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.projects");
    list.appendChild(empty);
    return;
  }

  const label = document.createElement("p");
  label.className = "list-label";
  label.textContent = t("label.sessions");
  list.appendChild(label);

  for (const p of PROJECTS) {
    const row = document.createElement("button");
    row.type = "button";
    row.className = "session";
    const stateTag =
      p.state === "error" ? "bad" : p.state === "syncing" ? "warn" : "ok";
    const total = Number(p.files_synced ?? 0) + Number(p.pending_outbox ?? 0);
    const pct = total > 0 ? Math.round((p.files_synced / total) * 100) : 100;
    row.innerHTML =
      `<span class="session-top">` +
      `<span class="session-name">${esc(p.display_name || p.project_id)}</span>` +
      `<span class="tag ${stateTag}">${esc(p.state ?? "?")}</span>` +
      `<span class="session-files">${esc(p.files_synced ?? 0)}/${esc(total || (p.files_synced ?? 0))}</span>` +
      `</span>` +
      `<span class="meter session-bar"><span class="meter-fill ${stateTag === "ok" ? "" : stateTag === "warn" ? "is-warn" : "is-bad"}" style="width:${Math.max(2, pct)}%"></span></span>`;
    row.addEventListener("click", () => {
      const sel = $("files-project");
      if (sel && PROJECTS.some((x) => x.project_id === p.project_id)) sel.value = p.project_id;
      showView("files", "files-filter");
      refreshFiles();
    });
    list.appendChild(row);
  }
  stagger(list, ".session");
}

function renderProjectsSettings() {
  const list = $("proj-list");
  list.textContent = "";
  if (!PROJECTS.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.projects");
    list.appendChild(empty);
    return;
  }
  for (const p of PROJECTS) {
    const div = document.createElement("div");
    div.className = "proj";
    const stateTag =
      p.state === "error" ? "bad" : p.state === "syncing" ? "warn" : "ok";
    const root = displayRoot(p.root_path ?? "");
    div.innerHTML =
      `<div class="proj-top">` +
      `<span class="proj-name">${esc(p.display_name || p.project_id)}</span>` +
      `<span class="tag ${stateTag}">${esc(p.state ?? "?")}</span>` +
      `<button type="button" class="btn btn-ghost btn-sm" data-detach="${esc(p.project_id)}">${esc(t("btn.detach"))}</button>` +
      `</div>` +
      `<div class="proj-root">` +
      `<code title="${esc(p.root_path ?? "")}">${esc(root)}</code>` +
      `<button type="button" class="btn btn-icon btn-copy" data-copy="${esc(p.root_path ?? "")}" aria-label="copy path">` +
      `<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-2a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 3.5v5A1.5 1.5 0 0 0 4 10h1.5"/></svg>` +
      `</button></div>` +
      (p.last_error ? `<p class="proj-err">${esc(p.last_error)}</p>` : "");
    div.querySelector("[data-detach]").addEventListener("click", async (ev) => {
      const pid = ev.currentTarget.dataset.detach;
      if (!confirm(t("confirm.detach", { p: pid }))) return;
      const r = await postJSON("/api/v1/detach", { project_id: pid });
      if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
      else toast(t("toast.detached"));
      refreshAll();
    });
    const copyBtnEl = div.querySelector(".btn-copy");
    if (copyBtnEl) copyBtnEl.addEventListener("click", copyBtn);
    list.appendChild(div);
  }
}

async function refreshProjects() {
  try {
    const r = await getJSON("/api/v1/projects");
    PROJECTS = r.projects || [];
    fillProjectSelects();
    renderSessions();
    renderProjectsSettings();
    const leaf = $("crumb-leaf");
    leaf.textContent =
      PROJECTS.length === 0
        ? t("head.noRoots")
        : PROJECTS.length === 1
          ? (PROJECTS[0].display_name || PROJECTS[0].project_id)
          : t("head.roots", { n: PROJECTS.length });
    leaf.title = PROJECTS.length === 1 ? displayRoot(PROJECTS[0].root_path || "") : "";
  } catch { /* covered by the state chip */ }
}

/* ============================== files ============================== */

/* status glyphs: check (ok), pulse (in flight), alert (conflict),
   cloud-off (placeholder). One stroke weight, 14px, currentColor. */
const ST_ICONS = {
  ok: '<svg class="ic st-ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="8" cy="8" r="6.4"/><path d="M5.4 8.2l1.8 1.8 3.4-3.8"/></svg>',
  warn: '<svg class="ic st-ic spin" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.7-3.75"/><path d="M13 2.6v2.4h-2.4"/></svg>',
  bad: '<svg class="ic st-ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 1.9l6.3 11H1.7z" stroke-linejoin="round"/><path d="M8 6.2v3.2"/><circle cx="8" cy="11.6" r="0.4" fill="currentColor" stroke="none"/></svg>',
  dim: '<svg class="ic st-ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M10.6 10.9a2.4 2.4 0 0 1-.2-4.8 3.6 3.6 0 0 1 7 .7 2.6 2.6 0 0 1-1.5 4.1z"/><path d="M5.6 12.4l3.6 2.1M9.2 14.5l1-1.7"/></svg>',
};

const PIN_IC = '<svg class="ic pin-ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9.6 2.3l4.1 4.1-1.1 1.1-1-.3-2.9 2.9.3 2.2-1 1-3-3-3.4 3.4 3.4-3.4-3-3 1-1 2.2.3 2.9-2.9-.3-1z"/></svg>';
const DOC_IC = '<svg class="ic f-ic f-doc" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.1" aria-hidden="true"><path d="M4 1.8h5.2L12 4.6V14.2H4z" stroke-linejoin="round"/><path d="M9.2 1.8v2.8H12" stroke-linejoin="round"/></svg>';

function stFor(f) {
  const st = f.state || "syncing";
  if (st === "synced") return ["ok", "files.synced"];
  if (st === "conflict") return ["bad", "files.conflict"];
  if (st === "syncing") return ["warn", "files.syncing"];
  return ["dim", "files.placeholder"];
}

function fileActionBtn(kind, label, path) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "btn btn-icon";
  btn.setAttribute("aria-label", label);
  btn.title = label;
  btn.dataset.path = path;
  if (kind === "pin") {
    btn.innerHTML = PIN_IC;
    btn.dataset.act = "pin";
  } else if (kind === "copy") {
    btn.innerHTML =
      '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-2A1.5 1.5 0 0 0 9 2H4a1.5 1.5 0 0 0-1.5 1.5v5A1.5 1.5 0 0 0 4 10h1.5"/></svg>';
    btn.dataset.act = "copy";
  } else {
    btn.innerHTML =
      '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.5v8M4.75 7.25L8 10.5l3.25-3.25M2.75 13.25h10.5"/></svg>';
    btn.dataset.act = "recall";
  }
  return btn;
}

function renderFilesSkeleton() {
  const body = $("files-body");
  body.innerHTML = "";
  for (let i = 0; i < 9; i++) {
    const tr = document.createElement("tr");
    tr.className = "f-skel";
    tr.innerHTML =
      `<td><span class="skel" style="width:${34 + ((i * 29) % 42)}%; display:inline-block"></span></td>` +
      `<td class="num"><span class="skel" style="width:44px; display:inline-block"></span></td>` +
      `<td><span class="skel" style="width:66px; display:inline-block"></span></td>` +
      `<td></td>`;
    body.appendChild(tr);
  }
}

function renderFilesError() {
  const body = $("files-body");
  body.innerHTML =
    `<tr><td colspan="4" class="f-error">` +
    `<svg class="f-error-mark" width="56" height="56" viewBox="0 0 24 24" xmlns="http://www.w3.org/2000/svg" aria-hidden="true">` +
    `<rect x="9" y="3.5" width="6" height="4" rx="1.2" fill="currentColor" opacity="0.35"/>` +
    `<rect x="6.5" y="9" width="11" height="4" rx="1.2" fill="currentColor" opacity="0.5"/>` +
    `<rect x="3.5" y="14.5" width="17" height="4" rx="1.2" fill="currentColor" opacity="0.65"/></svg>` +
    `<p class="f-error-note">${esc(t("files.error"))}</p></td></tr>`;
}

function renderFiles(r) {
  const body = $("files-body");
  const sum = $("files-summary");
  if (!body) return;
  body.innerHTML = "";
  if (!r || r.ok !== true || !r.files || r.files.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.files"))}</td></tr>`;
    sum.textContent = "";
    FOOT.files = 0;
    FOOT.synced = 0;
    renderFooter();
    return;
  }
  const s = r.summary || {};
  FOOT.files = Number(s.files) || 0;
  FOOT.synced = Number(s.synced) || 0;
  FOOT.conflicts = Number(s.conflict) || 0;
  paintState();

  sum.textContent = t("files.summary", {
    files: s.files ?? 0, synced: s.synced ?? 0, syncing: s.syncing ?? 0, conflict: s.conflict ?? 0,
  });

  const project = selectedProject();

  for (const f of r.files.slice(0, 300)) {
    const [dir, base] = splitPath(f.path);
    const [cls, key] = stFor(f);
    const tr = document.createElement("tr");

    const tdName = document.createElement("td");
    tdName.innerHTML =
      `<span class="f-name">${DOC_IC}<span class="f-base" title="${esc(f.path)}">${esc(base)}</span></span>` +
      (dir ? `<span class="f-dir">${esc(dir)}</span>` : "");

    const tdSize = document.createElement("td");
    tdSize.className = "num";
    tdSize.textContent = fmtBytes(f.size);

    const tdState = document.createElement("td");
    tdState.innerHTML =
      `<span class="f-state s-${cls}">${ST_ICONS[cls]}<span>${esc(t(key))}</span>` +
      (f.pinned ? `<span class="f-pinmark" title="${esc(t("files.pinnedA11y"))}">${PIN_IC}</span>` : "") +
      `</span>`;

    const tdAct = document.createElement("td");
    const rowActions = document.createElement("div");
    rowActions.className = "row-actions";
    const pinBtn = fileActionBtn("pin", f.pinned ? t("btn.unpin") : t("btn.pin"), f.path);
    pinBtn.dataset.act = f.pinned ? "unpin" : "pin";
    pinBtn.style.color = f.pinned ? "var(--warn-fg)" : "";
    rowActions.appendChild(pinBtn);
    rowActions.appendChild(fileActionBtn("copy", t("btn.copy"), f.path));
    if (f.placeholder) rowActions.appendChild(fileActionBtn("recall", t("btn.recall"), f.path));
    tdAct.appendChild(rowActions);

    pinBtn.addEventListener("click", () => doFilePin(project, f.path, f.pinned));
    tdAct.querySelector('[data-act="copy"]').addEventListener("click", copyBtn);
    const recallBtnEl = tdAct.querySelector('[data-act="recall"]');
    if (recallBtnEl) recallBtnEl.addEventListener("click", () => doRecall(project, f.path));

    tr.append(tdName, tdSize, tdState, tdAct);
    body.appendChild(tr);
  }
}

function selectedProject() {
  const el = $("files-project");
  if (el && el.value) return el.value;
  return PROJECTS.length > 0 ? PROJECTS[0].project_id : "";
}

async function refreshFiles() {
  const project = selectedProject();
  if (!project) { LAST_FILES = null; renderFiles(null); return; }
  const filter = $("files-filter").value.trim();
  const key = `${project}::${filter}`;
  if (FILES_KEY !== key) {
    FILES_KEY = key;
    renderFilesSkeleton();
  }
  const q = filter ? `&q=${encodeURIComponent(filter)}` : "";
  try {
    LAST_FILES = await getJSON(`/api/v1/files?project=${encodeURIComponent(project)}${q}`);
    renderFiles(LAST_FILES);
  } catch {
    if (!LAST_FILES || LAST_FILES.ok !== true) renderFilesError();
  }
}

async function doFilePin(project, path, pinned) {
  if (!project || !path) return;
  const url = pinned ? "/api/v1/pins/unpin" : "/api/v1/pins";
  const r = await postJSON(url, { project_id: project, path });
  if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
  else toast(pinned ? t("toast.unpinned") : t("toast.pinned"));
  refreshFiles();
  refreshPins();
}

/* ============================== pinned assets (dashboard) ============================== */

async function refreshPins() {
  PIN_ROWS.length = 0;
  const targets = PROJECTS.slice(0, 4);
  await Promise.all(targets.map(async (p) => {
    try {
      const r = await getJSON(`/api/v1/pins?project=${encodeURIComponent(p.project_id)}`);
      if (r.ok !== true) return;
      for (const pin of r.pins || []) {
        PIN_ROWS.push({
          project: p.project_id,
          project_name: p.display_name || p.project_id,
          path: pin.path,
          size: pin.size,
          state: pin.state || "pinned",
        });
      }
    } catch { /* pins stay at their last honest value */ }
  }));
  renderAssets();
}

function renderAssets() {
  const body = $("assets-body");
  body.textContent = "";
  $("assets-count").textContent = PIN_ROWS.length ? String(PIN_ROWS.length) : "";
  if (!PIN_ROWS.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.assets");
    body.appendChild(empty);
    return;
  }
  for (const a of PIN_ROWS.slice(0, 12)) {
    const [, base] = splitPath(a.path);
    const row = document.createElement("div");
    row.className = "asset-row";
    row.innerHTML =
      `<span class="asset-name" title="${esc(a.path)}">${esc(base)}</span>` +
      `<span class="asset-side">` +
      `<span class="pinned" title="${esc(t("files.pinnedA11y"))}">${PIN_IC}</span>` +
      `<span>${esc(fmtBytes(a.size))}</span>` +
      `<button type="button" class="btn btn-icon btn-sm" data-unpin="${esc(a.path)}" aria-label="unpin">` +
      `<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true"><path d="M3.5 3.5l9 9M12.5 3.5l-9 9"/></svg>` +
      `</button></span>` +
      `<span class="asset-sub">${esc(a.project_name)} · ${esc(a.path)}</span>`;
    row.querySelector("[data-unpin]").addEventListener("click", () => {
      doFilePin(a.project, a.path, true);
    });
    body.appendChild(row);
  }
  stagger(body, ".asset-row");
}

/* ============================== journal trail + spark ============================== */

function renderActivity(entries) {
  LAST_ACTIVITY = entries || [];
  const trail = $("trail-list");
  trail.textContent = "";
  if (!LAST_ACTIVITY.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.activity");
    trail.appendChild(empty);
  } else {
    const label = document.createElement("p");
    label.className = "list-label";
    label.textContent = t("label.journal");
    trail.appendChild(label);
    for (const e of LAST_ACTIVITY.slice(-10).reverse()) {
      const row = document.createElement("div");
      row.className = "trail-row";
      const kind = e.kind || "upsert";
      // human words on the badge ("saved" beats "UPSERT"); the raw
      // kind stays available to anyone who reads the journal itself
      const kindKey = kind === "delete" ? "act.deleted" : kind === "rename" ? "act.renamed" : "act.saved";
      const tag = kind === "delete" ? "bad" : kind === "rename" ? "info" : "ok";
      row.innerHTML =
        `<span class="trail-seq">${esc(e.seq ?? "-")}</span>` +
        `<span class="trail-path" title="${esc(e.path ?? "")}">${esc(e.path ?? "")}</span>` +
        `<span class="tag ${tag}">${esc(t(kindKey))}</span>` +
        `<span class="trail-size">${esc(fmtBytes(e.size))}</span>`;
      trail.appendChild(row);
    }
    stagger(trail, ".trail-row");
  }
  renderSpark(LAST_ACTIVITY);
}

/* honest volume viz: journal bytes, bucketed over the last entries.
   No invented days-of-week (the feed carries no timestamps); the
   label says exactly what it is. */
function renderSpark(entries) {
  const spark = $("spark");
  const note = $("spark-note");
  spark.textContent = "";
  const rows = (entries || []).slice(-14);
  if (!rows.length) {
    note.textContent = "";
    return;
  }
  const max = Math.max(...rows.map((e) => Number(e.size) || 0), 1);
  for (const e of rows) {
    const bar = document.createElement("span");
    bar.className = "spark-bar";
    const h = Math.max(3, Math.round(((Number(e.size) || 0) / max) * 30));
    bar.style.height = `${h}px`;
    bar.style.opacity = String(0.35 + (h / 30) * 0.65);
    spark.appendChild(bar);
  }
  note.textContent = t("spark.note", { n: rows.length });
}

/* ============================== locks ============================== */

function renderLocks(locks) {
  LAST_LOCKS = locks || [];
  const list = $("lock-list");
  list.textContent = "";
  if (!LAST_LOCKS.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.locks");
    list.appendChild(empty);
    return;
  }
  const now = Date.now();
  for (const l of LAST_LOCKS) {
    const remainMs = (l.expires_at ?? 0) - now;
    const live = remainMs > 0;
    const row = document.createElement("div");
    row.className = "lock-row";
    row.innerHTML =
      `<span class="lock-path" title="${esc(l.path ?? "")}">${esc(l.path ?? "")}</span>` +
      `<span class="tag ${live ? "ok" : "warn"}">${live ? "held" : "stale"}</span>` +
      `<span class="lock-when">${live ? `${Math.ceil(remainMs / 1000)}s` : "expired"} · ${esc(l.token ?? "-")}</span>`;
    list.appendChild(row);
  }
}

/* ============================== team ============================== */

function renderTeam(r) {
  const body = $("team-body");
  body.textContent = "";
  const projects = (r && r.projects) || [];
  if (!projects.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.team");
    body.appendChild(empty);
    return;
  }
  for (const p of projects) {
    const card = document.createElement("div");
    card.className = "team-project";

    const me = document.createElement("div");
    me.className = "team-me";
    me.innerHTML =
      `<span class="role-chip">${esc(t("team.myRole"))}: <b>${esc(p.my_role)}</b></span>` +
      `<span class="mono dim">${esc(p.my_device)}</span>`;
    card.appendChild(me);

    const table = document.createElement("table");
    table.className = "table";
    table.innerHTML =
      `<thead><tr><th>${esc(t("th.member"))}</th><th>${esc(t("th.role"))}</th></tr></thead><tbody></tbody>`;
    const rows = (p.members || []).slice().sort((a, b) => (a.is_me === b.is_me ? String(a.name).localeCompare(String(b.name)) : a.is_me ? -1 : 1));
    for (const m of rows) {
      const tr = document.createElement("tr");
      tr.innerHTML =
        `<td class="sans">${m.is_me ? `<span class="role-chip">${esc(t("team.you"))}</span> ` : ""}${esc(m.name || m.device_id)}</td>` +
        `<td class="sans"><span class="tag ${m.role === "Owner" ? "info" : ""}">${esc(m.role)}</span> <span class="mono dim">${esc(m.device_id)}</span></td>`;
      table.querySelector("tbody").appendChild(tr);
    }
    if (!rows.length) {
      table.querySelector("tbody").innerHTML = `<tr><td colspan="2" class="empty">${esc(t("empty.team"))}</td></tr>`;
    }
    card.appendChild(table);

    if (p.join_code) {
      const invite = document.createElement("div");
      invite.className = "team-invite";
      invite.innerHTML =
        `<span class="invite-label">${esc(t("team.invite"))}</span>` +
        `<code class="join-code">${esc(p.join_code)}</code>` +
        `<button type="button" class="btn btn-ghost btn-sm btn-copy" data-copy="${esc(p.join_code)}">${esc(t("btn.copy"))}</button>`;
      invite.querySelector(".btn-copy").addEventListener("click", copyBtn);
      card.appendChild(invite);
    }

    if (p.audit && p.audit.length) {
      const audit = document.createElement("div");
      audit.innerHTML = `<p class="note" style="margin:12px 0 0">${esc(t("team.audit"))}</p>`;
      const list = document.createElement("ul");
      list.className = "audit-list";
      for (const e of p.audit) {
        const li = document.createElement("li");
        const allow = e.allowed === true;
        li.innerHTML =
          `<span class="tag ${allow ? "ok" : "bad"}">${esc(allow ? t("team.allowed") : t("team.denied"))}</span>` +
          `<span class="audit-action">${esc(e.action)}</span>` +
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

/* ============================== versions ============================== */

function renderSnapshots(snapshots) {
  const body = $("snapshot-body");
  body.innerHTML = "";
  if (!snapshots || snapshots.length === 0) {
    body.innerHTML = `<tr><td colspan="3" class="empty">${esc(t("empty.versions"))}</td></tr>`;
    return;
  }
  for (const s of snapshots.slice(0, 10)) {
    const tr = document.createElement("tr");
    tr.innerHTML =
      `<td class="sans">${esc(s.label || (s.commit_hash || "").slice(0, 10))}</td>` +
      `<td class="sans dim">${esc(s.author || "")}</td>` +
      `<td class="sans"><button type="button" class="btn btn-ghost btn-sm" data-restore="${esc(s.commit_hash)}">${esc(t("btn.restore"))}</button></td>`;
    tr.querySelector("[data-restore]").addEventListener("click", async (ev) => {
      const project = selectedProject();
      if (!project) return;
      if (!confirm(t("confirm.restore"))) return;
      const r = await postJSON("/api/v1/snapshots/restore", {
        project_id: project,
        commit_hash: ev.currentTarget.dataset.restore,
      });
      if (r.ok) toast(t("toast.restored", { n: r.restored_files, b: fmtBytes(r.bytes) }));
      else toast(t("toast.failed", { e: r.error }), true);
      refreshAll();
    });
    body.appendChild(tr);
  }
}

async function refreshSnapshots() {
  const project = selectedProject();
  if (!project) return;
  try {
    const r = await getJSON(`/api/v1/snapshots?project=${encodeURIComponent(project)}`);
    renderSnapshots(r.ok ? r.snapshots : []);
  } catch { /* empty stays honest */ }
}

async function doSnapshot() {
  const project = selectedProject();
  if (!project) return;
  const r = await postJSON("/api/v1/snapshots", {
    project_id: project,
    label: $("snapshot-label").value.trim(),
  });
  if (r.ok) {
    $("snapshot-label").value = "";
    toast(t("toast.versionCreated"));
    refreshSnapshots();
  } else toast(t("toast.failed", { e: r.error }), true);
}

/* ============================== recall ============================== */

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
      `<div class="recall-head"><span class="mono">${esc(id.slice(0, 8))}</span>` +
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
      if (r.ok) RECALL_JOBS.set(id, r);
    } catch { /* keep last state */ }
  }
  renderRecallJobs();
}

async function doRecall(project, path) {
  const pid = project || selectedProject();
  if (!pid) return;
  const r = await postJSON("/api/v1/recall", {
    project_id: pid,
    path: path !== undefined ? path : $("recall-path").value.trim(),
  });
  if (r.ok) {
    RECALL_JOBS.set(r.job_id, { state: "running", progress: 0 });
    renderRecallJobs();
    toast(t("toast.recallStarted"));
    openPanel("panel-recall");
  } else toast(t("toast.failed", { e: r.error }), true);
}

/* ============================== flags ============================== */

function renderFlags(flags) {
  const grid = $("flag-grid");
  grid.innerHTML = "";
  for (const f of flags || []) {
    const on = String(f.value).toLowerCase() !== "false";
    const div = document.createElement("button");
    div.type = "button";
    div.className = "flag" + (on ? " on" : "");
    div.setAttribute("role", "switch");
    div.setAttribute("aria-checked", String(on));
    div.dataset.name = f.name;
    div.dataset.next = on ? "false" : "true";
    div.innerHTML =
      `<span class="flag-name">${esc(f.name)}</span>` +
      `<span class="flag-state">${f.name === "placeholder_driver" ? esc(f.value) : on ? "on" : "off"}</span>`;
    div.addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      const r = await postJSON("/api/v1/flags", { name: btn.dataset.name, value: btn.dataset.next });
      if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
      refreshAll();
    });
    grid.appendChild(div);
  }
}

/* ============================== doctor ============================== */

function renderDoctor(report) {
  const box = $("doctor-body");
  // keep the hydration row (prepended by refreshStatus) if present
  const i1row = box.querySelector("[data-i1]");
  box.textContent = "";
  if (i1row) box.appendChild(i1row);
  for (const c of (report && report.checks) || []) {
    const div = document.createElement("div");
    div.className = "check";
    const ms = Number(c.latency_ms ?? c.ms);
    div.innerHTML =
      `<span class="check-name">${esc(c.name)}</span>` +
      `<span class="check-ms">${Number.isFinite(ms) ? ms.toFixed(1) : "-"} ms</span>` +
      `<span class="check-detail">${esc(c.detail)}</span>`;
    box.appendChild(div);
  }
}

async function refreshOnce() {
  try {
    const d = await getJSON("/api/v1/doctor");
    renderDoctor(d);
  } catch { /* daemon down: the state chip reports it */ }
}

/* ============================== review strip (dashboard) ============================== */

function renderReview(rows) {
  const strip = $("review-strip");
  LAST_REVIEW = rows || [];
  const live = LAST_REVIEW.filter((r) => r.title !== null && r.title !== undefined);
  strip.textContent = "";
  strip.hidden = !live.length;
  if (!live.length) return;
  for (const r of live) {
    const v = (r.versions || []).slice(-1)[0];
    const el = document.createElement("span");
    el.className = "review-row";
    el.innerHTML =
      `<span class="tag info">${esc(t("review.label"))}</span>` +
      `<b>${esc(r.title)}</b>` +
      (v ? `<span class="mono dim">v${esc(v.number)} · ${esc(v.label || "")}</span>` : "") +
      `<span class="dim">${esc(t("review.notes", { n: r.open_notes ?? 0 }))}</span>`;
    strip.appendChild(el);
  }
}

async function refreshReview() {
  try {
    const r = await getJSON("/api/v1/review");
    renderReview(r.review || []);
  } catch { /* dashboard keeps polling */ }
}

/* ============================== live presence ============================== */

let LIVE_SSE = null;
const LIVE_ROWS = new Map(); // from -> {project, editor, frame, rate, action, at}

function liveRow(ev) {
  let payload = {};
  try { payload = JSON.parse(ev.payload || "{}"); } catch { /* foreign schema */ }
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
  const strip = $("presence-strip");
  strip.textContent = "";
  if (!LIVE_ROWS.size) { strip.hidden = true; return; }
  strip.hidden = false;
  const label = document.createElement("p");
  label.className = "presence-row";
  label.innerHTML = `<span class="dim mono">${esc(t("presence.live", { n: LIVE_ROWS.size }))}</span>`;
  strip.appendChild(label);
  const rows = [...LIVE_ROWS.values()].sort((a, b) => (a.local === b.local ? String(a.editor).localeCompare(String(b.editor)) : a.local ? -1 : 1));
  for (const r of rows) {
    const li = document.createElement("p");
    li.className = "presence-row";
    const tc = r.frame !== null && r.rate
      ? `${Math.floor(r.frame / (r.rate * 3600))}:${String(Math.floor((r.frame / (r.rate * 60)) % 60)).padStart(2, "0")}:${String(Math.floor((r.frame / r.rate) % 60)).padStart(2, "0")}:${String(Math.floor(r.frame % r.rate)).padStart(2, "0")}`
      : "-";
    li.innerHTML =
      `<span class="dot ${r.local ? "dot-ok" : ""}"></span>` +
      `<span><b>${esc(r.editor || r.from)}</b>${r.local ? ` (${esc(t("team.you"))})` : ""}</span>` +
      `<span class="mono">${esc(tc)}</span>` +
      `<span class="dim">${esc(r.action || "")}</span>` +
      `<span class="mono dim">${esc(r.project)}</span>`;
    strip.appendChild(li);
  }
}

function liveSseOpen() {
  if (LIVE_SSE) return;
  try {
    LIVE_SSE = new EventSource("/api/v1/live");
    LIVE_SSE.onmessage = (msg) => {
      try {
        const ev = JSON.parse(msg.data);
        LIVE_ROWS.set(ev.from, liveRow(ev));
        for (const [k, r] of LIVE_ROWS) if (Date.now() - r.at > 15000) LIVE_ROWS.delete(k);
        renderLive();
      } catch { /* skip malformed event */ }
    };
    LIVE_SSE.onerror = () => { /* stream closed: next refresh re-opens */ };
  } catch { /* EventSource unavailable - snapshot polling still covers */ }
}

async function refreshLive() {
  try {
    const snap = await getJSON("/api/v1/live/snapshot");
    const note = $("live-note");
    if (snap.enabled !== true) {
      if (LIVE_SSE) { LIVE_SSE.close(); LIVE_SSE = null; }
      LIVE_ROWS.clear();
      renderLive();
      // ONE honest line, in Settings next to the flags - never duplicated
      if (note) { note.hidden = false; note.textContent = t("live.off"); }
      return;
    }
    if (note) { note.hidden = false; note.textContent = t("note.live"); }
    for (const p of snap.projects || []) {
      for (const ev of p.events || []) {
        LIVE_ROWS.set(ev.from, liveRow({ ...ev, project: p.project, local: false }));
      }
    }
    liveSseOpen();
    renderLive();
  } catch { /* daemon gone - state chip covers */ }
}

/* ============================== search ============================== */

let searchTimer = null;

async function runSearch(q) {
  if (!q.trim()) { $("search-drop").hidden = true; return; }
  try {
    const r = await getJSON(`/api/v1/search?q=${encodeURIComponent(q)}`);
    const drop = $("search-drop");
    const results = (r && r.results) || [];
    if (!results.length) {
      drop.innerHTML = `<div class="sr-none">no matches for "${esc(q)}"</div>`;
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
          const view = TARGET_MAP[s.target] || (s.kind === "file" || s.kind === "project" ? "files" : "dashboard");
          showView(view);
          if (s.kind === "file" && $("files-filter")) {
            $("files-filter").value = s.label;
            refreshFiles();
          } else if (s.kind === "project" && $("files-project")) {
            if (PROJECTS.some((p) => p.project_id === s.project)) $("files-project").value = s.project;
            refreshFiles();
          }
        });
        drop.appendChild(row);
      }
    }
    drop.hidden = false;
  } catch { /* search is best-effort */ }
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

/* ============================== panels ============================== */

const PANELS = ["panel-add", "panel-history", "panel-recall"];

function openPanel(id) {
  closePanels();
  const panel = $(id);
  if (!panel) return;
  $("scrim").hidden = false;
  panel.hidden = false;
  const first = panel.querySelector("input, button.panel-close");
  if (first) first.focus();
}

function closePanels() {
  $("scrim").hidden = true;
  for (const id of PANELS) $(id).hidden = true;
}

document.querySelectorAll(".panel [data-close]").forEach((b) => {
  b.addEventListener("click", closePanels);
});
$("scrim").addEventListener("click", closePanels);

$("btn-add").addEventListener("click", () => openPanel("panel-add"));
$("btn-add-2").addEventListener("click", () => openPanel("panel-add"));
$("btn-history").addEventListener("click", () => {
  refreshSnapshots();
  openPanel("panel-history");
});
$("btn-recall-open").addEventListener("click", () => openPanel("panel-recall"));

/* ============================== help overlay ============================== */

function toggleHelp(force) {
  const ov = $("help-overlay");
  ov.hidden = force !== undefined ? !force : !ov.hidden;
}
$("help-close").addEventListener("click", () => toggleHelp(false));
$("foot-help").addEventListener("click", () => toggleHelp(true));

/* ============================== keyboard ============================== */

let gPending = false;
document.addEventListener("keydown", (ev) => {
  const tag = (ev.target && ev.target.tagName) || "";
  const typing = tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
  if (ev.key === "Escape") {
    closePanels();
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
    const map = { d: "dashboard", f: "files", s: "settings" };
    const view = map[ev.key.toLowerCase()];
    if (view) {
      ev.preventDefault();
      showView(view);
    }
    gPending = false;
  }
});

/* ============================== copy buttons ============================== */

function copyBtn(ev) {
  const text = ev.currentTarget.dataset.copy || "";
  navigator.clipboard
    .writeText(text)
    .then(() => toast(t("toast.copied")))
    .catch(() => {});
}

/* ============================== actions ============================== */

async function doAttach(rootEl, projectEl) {
  const root = (rootEl && rootEl.value.trim()) || "";
  if (!root) {
    if (rootEl) rootEl.focus();
    return;
  }
  const project = projectEl ? projectEl.value.trim() : "";
  const r = await postJSON("/api/v1/attach", { root_path: root, project_id: project });
  if (!r.ok) toast(t("toast.failed", { e: r.error }), true);
  else {
    toast(t("toast.attached"));
    rootEl.value = "";
    if (projectEl) projectEl.value = "";
    closePanels();
  }
  refreshAll();
}

$("btn-attach").addEventListener("click", () => doAttach($("attach-root"), $("attach-project")));
$("attach-cli-copy").addEventListener("click", copyBtn);
$("ob-cli-copy").addEventListener("click", copyBtn);
$("ob-attach-btn").addEventListener("click", () => doAttach($("ob-root"), null));
$("ob-root").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") { ev.preventDefault(); doAttach($("ob-root"), null); }
});

$("btn-snapshot").addEventListener("click", doSnapshot);
$("btn-recall").addEventListener("click", () => doRecall());

$("files-filter").addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(refreshFiles, 220);
});
$("files-project").addEventListener("change", refreshFiles);

/* ============================== orchestration ============================== */

async function refreshAll() {
  await refreshStatus();
  await refreshProjects();
  renderOnboarding();
  await refreshStorage();
  await refreshFiles();
  try {
    const feed = await getJSON("/api/v1/feed");
    renderActivity(feed.activity || []);
    renderLocks(feed.leases || []);
  } catch { /* covered by the state chip */ }
  await refreshPins();
  await refreshSnapshots();
  await pollRecallJobs();
  try {
    const f = await getJSON("/api/v1/flags");
    renderFlags(f.flags);
  } catch { /* covered */ }
  const node = $("set-node");
  if (node && DAEMON_UP !== false) node.textContent = t("node.online");
  refreshLive();
}

/* re-render the dynamic surfaces after a language switch */
function rerenderAllDynamic() {
  renderOnboarding();
  paintState();
  renderSessions();
  renderProjectsSettings();
  renderAssets();
  renderActivity(LAST_ACTIVITY);
  renderLocks(LAST_LOCKS);
  renderReview(LAST_REVIEW);
  renderFiles(LAST_FILES);
  renderRecallJobs();
  refreshTeam();
  refreshSnapshots();
  refreshOnce();
}

/* view from the URL hash on boot (deep links still land) */
(function bootRoute() {
  const h = (location.hash || "").replace("#", "");
  showView(VIEWS.includes(h) ? h : "dashboard");
})();

/* card cascade indexes */
stagger(document.querySelector(".dash-grid"), ".card");
stagger(document.querySelector(".settings-col"), ".card");

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
