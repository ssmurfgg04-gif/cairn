/* cairn local console - the plain-language pass (Task 2-b).
   No build step, no framework, no fake data: empty states stay empty
   until real data exists, every server string is escaped before it
   touches innerHTML, and ONE state derivation feeds every dot on
   screen (topbar chip, Home status card, footer - they can never
   disagree).

   Views: Home ("is everything okay?" + overview), Files (focused
   workspace with per-row ••• menus), People (roster + who's editing),
   Settings, How-to, Connect, Review, Merge - the last five grouped
   under the rail's More disclosure. Slide-over panels carry the
   workflows (add / versions / make available). Poll cadence: 2s core
   (presence/sync), 15s slow tier (storage/chart/versions), plus
   review 5s, team 8s, update 30s, doctor 15s, connect 8s. All fetches
   carry an AbortController timeout; all mutating buttons go busy(). */

"use strict";

const $ = (id) => document.getElementById(id);

/* ============================== i18n ==============================
   Four languages, zero em-dashes (taste-skill hard ban: the dash
   character is forbidden in visible copy; "·" and "-" carry). */

const STR = {
  "a11y.skip": { en: "skip to content", "de-DE": "zum Inhalt springen", "ja-JP": "本文へスキップ", "zh-CN": "跳到正文" },
  "a11y.theme": { en: "switch dark or light", "de-DE": "dunkel oder hell wechseln", "ja-JP": "ダーク/ライトを切り替え", "zh-CN": "切换深色或浅色" },
  "a11y.lang": { en: "language", "de-DE": "Sprache", "ja-JP": "言語", "zh-CN": "语言" },
  "a11y.close": { en: "close", "de-DE": "schließen", "ja-JP": "閉じる", "zh-CN": "关闭" },
  "a11y.help": { en: "shortcuts and cheatsheet", "de-DE": "Tastenkürzel und Spickzettel", "ja-JP": "ショートカットと早見表", "zh-CN": "快捷键与速查表" },
  "a11y.rowMenu": { en: "More actions", "de-DE": "Weitere Aktionen", "ja-JP": "その他の操作", "zh-CN": "更多操作" },
  "a11y.copyPath": { en: "copy path", "de-DE": "Pfad kopieren", "ja-JP": "パスをコピー", "zh-CN": "复制路径" },

  /* nav: exactly three primary destinations (review #13/#14), the rest
     lives under More. People is the roster + who's-editing surface. */
  "nav.home": { en: "Home", "de-DE": "Start", "ja-JP": "ホーム", "zh-CN": "主页" },
  "nav.files": { en: "Files", "de-DE": "Dateien", "ja-JP": "ファイル", "zh-CN": "文件" },
  "nav.people": { en: "People", "de-DE": "Personen", "ja-JP": "メンバー", "zh-CN": "成员" },
  "nav.more": { en: "More", "de-DE": "Mehr", "ja-JP": "その他", "zh-CN": "更多" },
  "nav.review": { en: "Review", "de-DE": "Review", "ja-JP": "レビュー", "zh-CN": "审阅" },
  "nav.merge": { en: "Merge", "de-DE": "Zusammenführen", "ja-JP": "マージ", "zh-CN": "合并" },
  "nav.connect": { en: "Connect", "de-DE": "Verbinden", "ja-JP": "接続", "zh-CN": "连接" },
  "nav.settings": { en: "Settings", "de-DE": "Einstellungen", "ja-JP": "設定", "zh-CN": "设置" },
  "nav.howto": { en: "How to use", "de-DE": "Anleitung", "ja-JP": "使い方", "zh-CN": "使用指南" },

  "btn.browse": { en: "Browse for folder...", "de-DE": "Ordner auswählen...", "ja-JP": "フォルダーを選ぶ...", "zh-CN": "选择文件夹..." },
  "or.type": { en: "or type a path", "de-DE": "oder Pfad eingeben", "ja-JP": "またはパスを入力", "zh-CN": "或输入路径" },
  "btn.open": { en: "Open", "de-DE": "Öffnen", "ja-JP": "開く", "zh-CN": "打开" },
  "btn.download": { en: "Download", "de-DE": "Herunterladen", "ja-JP": "ダウンロード", "zh-CN": "下载" },
  "btn.duplicate": { en: "Duplicate", "de-DE": "Duplizieren", "ja-JP": "複製", "zh-CN": "复制副本" },
  "btn.share": { en: "Share", "de-DE": "Teilen", "ja-JP": "共有", "zh-CN": "分享" },
  "btn.shareLink": { en: "Share project link", "de-DE": "Projekt-Link teilen", "ja-JP": "プロジェクトリンクを共有", "zh-CN": "分享项目链接" },
  "btn.makeAvailable": { en: "Make available", "de-DE": "Verfügbar machen", "ja-JP": "利用可能にする", "zh-CN": "设为可用" },
  "btn.keepHere": { en: "Keep on this computer", "de-DE": "Auf diesem Rechner behalten", "ja-JP": "このパソコンに保持", "zh-CN": "保留在此电脑" },
  "btn.stopKeeping": { en: "Stop keeping it", "de-DE": "Nicht mehr behalten", "ja-JP": "保持をやめる", "zh-CN": "不再保留" },
  "btn.editingCopy": { en: "Make editing copy", "de-DE": "Schnittkopie erstellen", "ja-JP": "編集用コピーを作成", "zh-CN": "创建编辑副本" },
  "btn.removeFolder": { en: "Remove folder", "de-DE": "Ordner entfernen", "ja-JP": "フォルダーを削除", "zh-CN": "移除文件夹" },
  "btn.restoreThis": { en: "Restore this version", "de-DE": "Diese Version wiederherstellen", "ja-JP": "このバージョンを復元", "zh-CN": "恢复此版本" },
  "btn.retry": { en: "Try again", "de-DE": "Erneut versuchen", "ja-JP": "再試行", "zh-CN": "重试" },
  "btn.search": { en: "Search", "de-DE": "Suchen", "ja-JP": "検索", "zh-CN": "搜索" },
  "toast.openedFolder": { en: "opened in your files", "de-DE": "in Dateien geöffnet", "ja-JP": "ファイルで開きました", "zh-CN": "已在文件管理器中打开" },
  "toast.duplicated": { en: "copied beside the original", "de-DE": "neben dem Original kopiert", "ja-JP": "元の隣にコピーしました", "zh-CN": "已复制到原文件旁" },
  "toast.copiedPath": { en: "path copied - paste it in chat or email", "de-DE": "Pfad kopiert - im Chat oder E-Mail einfügen", "ja-JP": "パスをコピーしました", "zh-CN": "路径已复制 - 可粘贴到聊天或邮件" },
  "toast.pickCancelled": { en: "no folder chosen", "de-DE": "kein Ordner gewählt", "ja-JP": "フォルダー未選択", "zh-CN": "未选择文件夹" },
  "toast.pickUnavailable": { en: "folder picker unavailable here - type the path instead", "de-DE": "Ordnerauswahl hier nicht verfügbar - Pfad eingeben", "ja-JP": "ここではフォルダー選択不可 - パスを入力", "zh-CN": "此处无法打开文件夹选择器 - 请输入路径" },
  "toast.downloading": { en: "downloading...", "de-DE": "lade herunter...", "ja-JP": "ダウンロード中...", "zh-CN": "下载中..." },
  "toast.timeout": { en: "Cairn took too long to answer - try again in a moment.", "de-DE": "Cairn hat zu lange nicht geantwortet - versuche es gleich nochmal.", "ja-JP": "Cairn の応答に時間がかかっています - しばらく待って再度お試しください。", "zh-CN": "Cairn 响应超时 - 请稍后重试。" },
  "vol.store": { en: "store volume", "de-DE": "Speicher-Volume", "ja-JP": "ストア領域", "zh-CN": "存储卷" },
  "vol.project": { en: "{p} drive", "de-DE": "{p} Laufwerk", "ja-JP": "{p} ドライブ", "zh-CN": "{p} 所在盘" },
  "flags.lead": { en: "Flags apply on the next job run, no restart. Owner and lead only (RBAC enforced). Click a flag to see what it does.", "de-DE": "Flags gelten beim nächsten Lauf, kein Neustart. Nur Owner und Lead (RBAC). Klick zeigt die Erklärung.", "ja-JP": "フラグは次回ジョブから反映（再起動不要）。オーナーとリードのみ（RBAC）。クリックで説明を表示。", "zh-CN": "开关在下一个任务生效，无需重启。仅所有者与主管可用（RBAC）。点击开关查看说明。" },
  /* flag explanations live here so all four languages carry them; the
     keys mirror the daemon's flag names (renderFlagsInner) */
  "flags.packing_enabled.line": { en: "Deduplicates file data between writes - only what changed travels.", "de-DE": "Verwendet Dateidaten zwischen Schreibvorgängen mehrfach - nur Änderungen werden übertragen.", "ja-JP": "書き込み間でファイルデータを重複排除し、変更があった分だけ転送します。", "zh-CN": "在写入之间对文件数据去重 - 只传输变化的部分。" },
  "flags.packing_enabled.flip": { en: "Packing on: edits sync as small deltas instead of whole files.", "de-DE": "Packing an: Änderungen werden als kleine Deltas statt ganzer Dateien synchronisiert.", "ja-JP": "Packing オン：編集をファイル全体ではなく小さな差分として同期します。", "zh-CN": "Packing 已开启：编辑以小增量同步，而非整个文件。" },
  "flags.tiering_enabled.line": { en: "Moves rarely-used chunks off local disk to make room (pinned files stay).", "de-DE": "Verschiebt selten genutzte Stücke vom lokalen Speicher, um Platz zu schaffen (gehaltene Dateien bleiben).", "ja-JP": "使用頻度の低い断片をローカルから移動して空きを作ります（保持ファイルは残ります）。", "zh-CN": "将不常用的分片移出本地磁盘以腾出空间（保留的文件不动）。" },
  "flags.tiering_enabled.flip": { en: "Tiering on: cold data frees local space automatically.", "de-DE": "Tiering an: kalte Daten geben lokalen Platz automatisch frei.", "ja-JP": "Tiering オン：使用頻度の低いデータがローカル容量を自動的に解放します。", "zh-CN": "Tiering 已开启：冷数据自动释放本地空间。" },
  "flags.delta_fold_enabled.line": { en: "Folds journal history into periodic versions - the restore points.", "de-DE": "Faltet die Journal-Historie in periodische Versionen - die Wiederherstellungspunkte.", "ja-JP": "ジャーナル履歴を定期的なバージョン（復元ポイント）へまとめます。", "zh-CN": "把日志历史折叠成周期性版本 - 也就是还原点。" },
  "flags.delta_fold_enabled.flip": { en: "Delta folding on: history compacts into restore points.", "de-DE": "Delta-Folding an: Historie verdichtet sich zu Wiederherstellungspunkten.", "ja-JP": "Delta folding オン：履歴が復元ポイントへ圧縮されます。", "zh-CN": "Delta folding 已开启：历史压缩为还原点。" },
  "flags.compression_enabled.line": { en: "Squeezes chunks before they travel or land (zstd).", "de-DE": "Packt Stücke, bevor sie reisen oder landen (zstd).", "ja-JP": "転送・保存の前に断片を圧縮します（zstd）。", "zh-CN": "在传输或落盘前压缩分片（zstd）。" },
  "flags.compression_enabled.flip": { en: "Compression on: transfers and storage shrink.", "de-DE": "Kompression an: Übertragungen und Speicher schrumpfen.", "ja-JP": "圧縮オン：転送量と保存容量が減ります。", "zh-CN": "压缩已开启：传输与存储占用变小。" },
  "flags.placeholder_driver.line": { en: "How remote files appear locally: native (Explorer-integrated) or plain.", "de-DE": "Wie entfernte Dateien lokal erscheinen: nativ (Explorer-integriert) oder schlicht.", "ja-JP": "リモートファイルの表示方式：ネイティブ（エクスプローラー統合）またはシンプル。", "zh-CN": "远程文件在本地的呈现方式：原生（集成资源管理器）或普通。" },
  "flags.placeholder_driver.flip": { en: "Driver set - remote files now appear as chosen.", "de-DE": "Treiber gesetzt - entfernte Dateien erscheinen jetzt wie gewählt.", "ja-JP": "ドライバーを設定しました - リモートファイルは選択した方法で表示されます。", "zh-CN": "驱动已设置 - 远程文件将按所选方式显示。" },
  "flags.normalize_containers.line": { en: "Splits container media (MXF/QT) into per-track chunks - big streaming wins, newer path.", "de-DE": "Teilt Container-Medien (MXF/QT) in Spur-Stücke - große Streaming-Gewinne, neuerer Weg.", "ja-JP": "コンテナメディア（MXF/QT）をトラック別の断片に分割 - ストリーミングが大幅に高速化する新しい経路。", "zh-CN": "把容器媒体（MXF/QT）按轨道拆成分片 - 流式收益更大，路径较新。" },
  "flags.normalize_containers.flip": { en: "Container normalization on: media is chunked per track on the next pass.", "de-DE": "Container-Normalisierung an: Medien werden beim nächsten Durchlauf pro Spur gestückelt.", "ja-JP": "コンテナ正規化オン：次回のパスでメディアをトラック別に分割します。", "zh-CN": "容器规范化已开启：下次任务将按轨道拆分媒体。" },
  "flags.live_presence.line": { en: "Shows where teammates are scrubbing in the timeline, live (this device only).", "de-DE": "Zeigt live, wo Teammates in der Timeline scrubben (nur dieses Gerät).", "ja-JP": "チームメイトがタイムラインのどこを再生しているかをライブ表示（この端末のみ）。", "zh-CN": "实时显示队友在时间线上的位置（仅本设备）。" },
  "flags.live_presence.flip": { en: "Live presence ON for this device at the next swarm join.", "de-DE": "Live-Präsenz AN für dieses Gerät beim nächsten Swarm-Beitritt.", "ja-JP": "次回スワーム参加時、この端末のライブプレゼンスがオンになります。", "zh-CN": "下次加入群组时，本设备的实时状态将开启。" },
  "flags.semantic_merge.line": { en: "Auto-merges timeline re-cuts when editors touched different frames.", "de-DE": "Führt Timeline-Neuschnitte automatisch zusammen, wenn Editoren verschiedene Frames bearbeitet haben.", "ja-JP": "編集者が異なるフレームを触った場合、タイムラインの再カットを自動マージします。", "zh-CN": "当剪辑者改动的是不同帧时，自动合并时间线重剪。" },
  "flags.semantic_merge.flip": { en: "Semantic merge on: frame-disjoint re-cuts merge instead of conflicting.", "de-DE": "Semantic Merge an: frame-disjunkte Neuschnitte verschmelzen statt zu kollidieren.", "ja-JP": "セマンティックマージオン：フレームが重ならない再カットは競合ではなくマージされます。", "zh-CN": "语义合并已开启：帧不重叠的重剪会自动合并而非冲突。" },
  "flags.fec_parity.line": { en: "Sends error-correction parity with swarm transfers - lost pieces rebuild without re-sending.", "de-DE": "Sendet Fehlerkorrektur-Parität mit Swarm-Übertragungen - verlorene Stücke bauen sich ohne erneutes Senden auf.", "ja-JP": "スワーム転送に誤り訂正パリティを添付 - 失った断片を再送なしで復元します。", "zh-CN": "群组传输附带纠错校验 - 丢失的分片无需重传即可重建。" },
  "flags.fec_parity.flip": { en: "FEC parity on: single lost fragments heal locally at the next swarm join.", "de-DE": "FEC-Parität an: einzelne verlorene Fragmente heilen beim nächsten Swarm-Beitritt lokal.", "ja-JP": "FEC パリティオン：失った断片を次回スワーム参加時にローカルで修復します。", "zh-CN": "FEC 校验已开启：丢失的分片在下次加入群组时本地修复。" },
  "flags.off": { en: "{name} off", "de-DE": "{name} aus", "ja-JP": "{name} オフ", "zh-CN": "{name} 已关闭" },
  "err.clipboard": { en: "clipboard blocked", "de-DE": "Zwischenablage blockiert", "ja-JP": "クリップボードがブロックされました", "zh-CN": "剪贴板被拦截" },
  "adv.label": { en: "Advanced / Command line", "de-DE": "Erweitert / Kommandozeile", "ja-JP": "上級者向け / コマンドライン", "zh-CN": "高级 / 命令行" },

  /* ============ HOME: "is everything okay?" (review #13) ============
     ONE derivation (deriveState) feeds the chip, the footer dot and
     this card - three surfaces, one truth. */
  "home.ok.title": { en: "Everything is up to date", "de-DE": "Alles ist auf dem neuesten Stand", "ja-JP": "すべて最新です", "zh-CN": "一切已是最新" },
  "home.ok.sub": { en: "Your files match on every connected computer.", "de-DE": "Deine Dateien stimmen auf allen verbundenen Rechnern überein.", "ja-JP": "接続中のすべてのパソコンでファイルが一致しています。", "zh-CN": "所有已连接电脑上的文件均已一致。" },
  "home.warn.title": { en: "Working…", "de-DE": "Beschäftigt…", "ja-JP": "処理中…", "zh-CN": "处理中…" },
  "home.warn.sub": { en: "Cairn is copying files in the background. You can keep editing.", "de-DE": "Cairn kopiert im Hintergrund Dateien. Du kannst weiterarbeiten.", "ja-JP": "Cairn がバックグラウンドでファイルをコピーしています。そのまま作業を続けられます。", "zh-CN": "Cairn 正在后台复制文件。你可以继续工作。" },
  "home.attention.title": { en: "Needs attention", "de-DE": "Braucht Aufmerksamkeit", "ja-JP": "対応が必要です", "zh-CN": "需要处理" },
  "home.attention.sub": { en: "A few things need a quick decision from you.", "de-DE": "Ein paar Dinge brauchen eine kurze Entscheidung von dir.", "ja-JP": "いくつかの項目に簡単な判断が必要です。", "zh-CN": "有几项需要你快速处理。" },
  "home.down.title": { en: "Cairn isn't running", "de-DE": "Cairn läuft nicht", "ja-JP": "Cairn が起動していません", "zh-CN": "Cairn 未运行" },
  "home.down.sub": { en: "We're trying to restart it.", "de-DE": "Wir versuchen einen Neustart.", "ja-JP": "再起動を試みています。", "zh-CN": "正在尝试重启。" },
  "home.files": { en: "files", "de-DE": "Dateien", "ja-JP": "ファイル", "zh-CN": "个文件" },
  "home.computers": { en: "computers connected", "de-DE": "Rechner verbunden", "ja-JP": "台のパソコンが接続中", "zh-CN": "台电脑已连接" },
  "home.oneComputer": { en: "only this computer", "de-DE": "nur dieser Rechner", "ja-JP": "このパソコンのみ", "zh-CN": "仅此电脑" },
  "home.openFolder": { en: "Open folder", "de-DE": "Ordner öffnen", "ja-JP": "フォルダーを開く", "zh-CN": "打开文件夹" },
  "home.issue.files": { en: "{n} files aren't on this computer", "de-DE": "{n} Dateien sind nicht auf diesem Rechner", "ja-JP": "{n} 個のファイルがこのパソコンにありません", "zh-CN": "{n} 个文件不在这台电脑上" },
  "home.issue.files.one": { en: "{n} file isn't on this computer", "de-DE": "{n} Datei ist nicht auf diesem Rechner", "ja-JP": "{n} 個のファイルがこのパソコンにありません", "zh-CN": "{n} 个文件不在这台电脑上" },
  "home.issue.filesCta": { en: "Make them available", "de-DE": "Verfügbar machen", "ja-JP": "利用可能にする", "zh-CN": "设为可用" },
  "home.issue.conflict": { en: "{n} files changed in different places", "de-DE": "{n} Dateien haben sich an verschiedenen Orten geändert", "ja-JP": "{n} 個のファイルが別々の場所で変更されました", "zh-CN": "{n} 个文件在不同位置被修改" },
  "home.issue.conflict.one": { en: "{n} file changed in different places", "de-DE": "{n} Datei hat sich an verschiedenen Orten geändert", "ja-JP": "{n} 個のファイルが別々の場所で変更されました", "zh-CN": "{n} 个文件在不同位置被修改" },
  "home.issue.conflictCta": { en: "Merge or keep both", "de-DE": "Zusammenführen oder beide behalten", "ja-JP": "マージまたは両方を保持", "zh-CN": "合并或保留两者" },
  "home.issue.projects": { en: "{n} projects couldn't finish copying", "de-DE": "{n} Projekte konnten nicht fertig kopieren", "ja-JP": "{n} 件のプロジェクトがコピーを完了できませんでした", "zh-CN": "{n} 个项目未能完成复制" },
  "home.issue.projects.one": { en: "{n} project couldn't finish copying", "de-DE": "{n} Projekt konnte nicht fertig kopieren", "ja-JP": "{n} 件のプロジェクトがコピーを完了できませんでした", "zh-CN": "{n} 个项目未能完成复制" },
  "home.issue.projectsCta": { en: "Take a look", "de-DE": "Nachsehen", "ja-JP": "確認する", "zh-CN": "查看" },
  "home.issue.daemon": { en: "Cairn isn't running. We're trying to restart it.", "de-DE": "Cairn läuft nicht. Wir versuchen einen Neustart.", "ja-JP": "Cairn が起動していません。再起動を試みています。", "zh-CN": "Cairn 未运行。正在尝试重启。" },

  /* how-to guide (six tasks, review #26/#27): task language, one click
     per card; every terminal command lives in the per-card Advanced fold */
  "howto.title": { en: "How to use Cairn", "de-DE": "Cairn verwenden", "ja-JP": "Cairn の使い方", "zh-CN": "如何使用 Cairn" },
  "howto.lede": { en: "Cairn keeps your whole studio's work on every computer - no upload sites, no hard drives in bags. The six things below are everything you need.", "de-DE": "Cairn hält die Arbeit des ganzen Studios auf jedem Rechner - ohne Upload-Seiten, ohne Festplatten im Beutel. Diese sechs Schritte sind alles, was du brauchst.", "ja-JP": "Cairn はスタジオの作業をすべてのパソコンで同じに保ちます。アップロードサイトも、持ち運び HDD も不要。以下の 6 つの手順だけで使えます。", "zh-CN": "Cairn 让工作室的全部工作在每台电脑上保持一致 - 无需上传网站，无需来回拷硬盘。下面六个步骤就是全部。" },
  "howto.adv": { en: "Advanced / Command line", "de-DE": "Erweitert / Kommandozeile", "ja-JP": "上級者向け / コマンドライン", "zh-CN": "高级 / 命令行" },
  "howto.t1.title": { en: "Add your project", "de-DE": "Dein Projekt hinzufügen", "ja-JP": "プロジェクトを追加", "zh-CN": "添加你的项目" },
  "howto.t1.body": { en: "Click Add project folder and pick the folder where your project lives - the one with your footage.", "de-DE": "Klicke auf Projektordner hinzufügen und wähle den Ordner, in dem dein Projekt liegt - der mit deinem Material.", "ja-JP": "「プロジェクトフォルダーを追加」をクリックし、プロジェクト（素材）のあるフォルダーを選びます。", "zh-CN": "点击「添加项目文件夹」，选择项目所在的文件夹 - 放素材的那个。" },
  "howto.t1.note": { en: "Your teammates do the same on their computers and join with a code (see step 5).", "de-DE": "Deine Teammates machen dasselbe auf ihren Rechnern und treten mit einem Code bei (siehe Schritt 5).", "ja-JP": "チームメイトも各自のパソコンで同じ操作をし、コードで参加します（手順 5）。", "zh-CN": "队友在他们的电脑上做同样操作，并用邀请码加入（见第 5 步）。" },
  "howto.t2.title": { en: "Open your normal folder", "de-DE": "Deinen normalen Ordner öffnen", "ja-JP": "いつものフォルダーを開く", "zh-CN": "打开平时的文件夹" },
  "howto.t2.body": { en: "The project folder works like any folder on your computer. Open it in Explorer or Finder - Premiere and DaVinci see the same files.", "de-DE": "Der Projektordner funktioniert wie jeder andere Ordner. Öffne ihn im Explorer oder Finder - Premiere und DaVinci sehen dieselben Dateien.", "ja-JP": "プロジェクトフォルダーは普通のフォルダーと同じように使えます。エクスプローラーや Finder で開くと、Premiere と DaVinci が同じファイルを見ます。", "zh-CN": "项目文件夹和普通文件夹一样。在资源管理器或访达中打开 - Premiere 和 DaVinci 看到的是同样的文件。" },
  "howto.t2.note": { en: "Home → Open folder takes you straight there.", "de-DE": "Start → Ordner öffnen bringt dich direkt hin.", "ja-JP": "ホーム → 「フォルダーを開く」で直接開けます。", "zh-CN": "主页 →「打开文件夹」可直接前往。" },
  "howto.t3.title": { en: "Work normally", "de-DE": "Ganz normal arbeiten", "ja-JP": "いつもどおり作業する", "zh-CN": "正常工作" },
  "howto.t3.body": { en: "Edit, save, move files as you always do. Cairn copies every change to your other computers in the background.", "de-DE": "Schneide, speichere und verschiebe Dateien wie immer. Cairn kopiert jede Änderung im Hintergrund auf deine anderen Rechner.", "ja-JP": "編集・保存・移動はいつもどおり。Cairn がすべての変更をバックグラウンドで他のパソコンへコピーします。", "zh-CN": "像平常一样剪辑、保存、移动文件。Cairn 会在后台把每个改动复制到你的其他电脑。" },
  "howto.t3.note": { en: "The dot at the top says Working… while copies are in flight - you can keep editing.", "de-DE": "Der Punkt oben zeigt Beschäftigt…, während kopiert wird - du kannst weiterarbeiten.", "ja-JP": "コピー中は上部のドットが「処理中…」になります - 作業はそのまま続けられます。", "zh-CN": "后台复制时，顶部圆点会显示「处理中…」 - 你可以继续工作。" },
  "howto.t4.title": { en: "Make a file available offline", "de-DE": "Eine Datei offline verfügbar machen", "ja-JP": "ファイルをオフラインで使えるようにする", "zh-CN": "让文件可离线使用" },
  "howto.t4.body": { en: "Files that live on other computers show Online only in Files. Open the row's ••• menu and choose Make available - the full file copies onto this computer.", "de-DE": "Dateien auf anderen Rechnern erscheinen in Dateien als Nur online. Öffne das ••• Menü der Zeile und wähle Verfügbar machen - die volle Datei wird auf diesen Rechner kopiert.", "ja-JP": "他のパソコンにあるファイルは「ファイル」で「オンラインのみ」と表示されます。行の ••• メニューから「利用可能にする」を選ぶと、ファイル全体がこのパソコンにコピーされます。", "zh-CN": "存放在其他电脑的文件在「文件」中显示为「仅在线」。打开该行的 ••• 菜单并选择「设为可用」 - 完整文件会复制到这台电脑。" },
  "howto.t4.note": { en: "Going on a shoot or a flight? Also choose Keep on this computer so it never gets cleaned up.", "de-DE": "Dreh oder Flug geplant? Wähle zusätzlich Auf diesem Rechner behalten, damit sie nie automatisch wegräumt wird.", "ja-JP": "撮影や出張の予定がある場合は「このパソコンに保持」も選んでください。自動的に整理されなくなります。", "zh-CN": "要外出拍摄或坐飞机？同时选择「保留在此电脑」，它就不会被自动清理。" },
  "howto.t5.title": { en: "Share with a teammate", "de-DE": "Mit einem Teammate teilen", "ja-JP": "チームメイトと共有する", "zh-CN": "与队友共享" },
  "howto.t5.body": { en: "Open More → Connect, click Generate code, and send the code to your teammate. They paste it in the same place on their computer.", "de-DE": "Öffne Mehr → Verbinden, klicke Code erzeugen und schicke den Code deinem Teammate. Er wird bei ihm an derselben Stelle eingefügt.", "ja-JP": "「その他 → 接続」を開き、「コードを生成」をクリックしてコードをチームメイトに送ります。相手は同じ場所に貼り付けます。", "zh-CN": "打开「更多 → 连接」，点击「生成邀请码」，把码发给队友。对方在自己的电脑上同样位置粘贴。" },
  "howto.t5.note": { en: "Sharing one file instead? The row's ••• menu has Share project link.", "de-DE": "Nur eine Datei teilen? Das ••• Menü der Zeile hat Projekt-Link teilen.", "ja-JP": "1 つのファイルだけ共有したい場合は、行の ••• メニューの「プロジェクトリンクを共有」を使います。", "zh-CN": "只想分享一个文件？该行的 ••• 菜单里有「分享项目链接」。" },
  "howto.t6.title": { en: "Send a review link", "de-DE": "Einen Review-Link schicken", "ja-JP": "レビューリンクを送る", "zh-CN": "发送审阅链接" },
  "howto.t6.body": { en: "Open More → Review, publish the cut, then click Generate link. The client opens it in a browser and leaves notes pinned to exact frames. No account, no software.", "de-DE": "Öffne Mehr → Review, veröffentliche den Schnitt und klicke Link erzeugen. Der Client öffnet ihn im Browser und hinterlässt Notizen an exakten Frames. Kein Konto, keine Software.", "ja-JP": "「その他 → レビュー」を開き、カットを公開して「リンクを生成」をクリック。クライアントはブラウザーで開き、正確なフレームにメモを残せます。アカウントもソフトも不要。", "zh-CN": "打开「更多 → 审阅」，发布成片，然后点击「生成链接」。客户在浏览器中打开即可在精确帧上留言。无需账号，无需软件。" },
  "howto.t6.note": { en: "Pick how long the link stays open before you generate it.", "de-DE": "Lege vor dem Erzeugen fest, wie lange der Link offen bleibt.", "ja-JP": "リンクを生成する前に、有効期間を選べます。", "zh-CN": "生成链接前先选择有效期。" },
  "howto.faq.title": { en: "When something looks wrong", "de-DE": "Wenn etwas seltsam aussieht", "ja-JP": "うまく動かないときは", "zh-CN": "出现问题时" },
  "howto.faq.1": { en: "Working… dot - a copy is in flight or one computer is briefly offline. It fixes itself; give it a moment.", "de-DE": "Beschäftigt…-Punkt - eine Kopie läuft oder ein Rechner ist kurz offline. Das löst sich von selbst; gib ihm einen Moment.", "ja-JP": "「処理中…」のドット - コピー中か、一時的にオフラインのパソコンがあるだけです。自然に解決します。", "zh-CN": "「处理中…」圆点 - 正在复制，或有电脑短暂离线。会自动恢复，稍等即可。" },
  "howto.faq.2": { en: "Needs attention - the Home card lists exactly what to click. If a project folder was deleted or moved, remove the project under Projects & paths and add the folder again.", "de-DE": "Braucht Aufmerksamkeit - die Start-Karte sagt genau, was zu klicken ist. Wurde ein Projektordner gelöscht oder verschoben, entferne das Projekt unter Projekte & Pfade und füge den Ordner erneut hinzu.", "ja-JP": "「対応が必要」 - ホームのカードに、何をクリックすべきかが表示されます。プロジェクトフォルダーを削除・移動した場合は「プロジェクトとパス」でプロジェクトを取り除き、もう一度フォルダーを追加してください。", "zh-CN": "「需要处理」 - 主页卡片会列出具体要点什么。如果项目文件夹被删除或移动，请在「项目与路径」中移除该项目，再重新添加文件夹。" },
  "howto.faq.3": { en: "A file shows Online only - open its ••• menu and choose Make available; it copies from your other computers.", "de-DE": "Eine Datei zeigt Nur online - öffne ihr ••• Menü und wähle Verfügbar machen; sie wird von deinen anderen Rechnern kopiert.", "ja-JP": "ファイルが「オンラインのみ」 - ••• メニューから「利用可能にする」を選ぶと、他のパソコンからコピーされます。", "zh-CN": "文件显示「仅在线」 - 打开它的 ••• 菜单选择「设为可用」，会从你的其他电脑复制过来。" },
  "howto.faq.4": { en: "Nothing is moving - the green Cairn icon in the system tray (near the clock) restarts everything. Right-click it for status.", "de-DE": "Nichts bewegt sich - das grüne Cairn-Symbol in der Taskleiste (nahe der Uhr) startet alles neu. Rechtsklick zeigt den Status.", "ja-JP": "何も進まない - タスクバー（時計の近く）の緑の Cairn アイコンですべてを再起動できます。右クリックで状態を確認。", "zh-CN": "一切都没动静 - 系统托盘（时钟附近）的绿色 Cairn 图标可以重启一切。右键点击可查看状态。" },

  "head.noRoots": { en: "no project", "de-DE": "kein Projekt", "ja-JP": "プロジェクトなし", "zh-CN": "无项目" },
  "head.roots": { en: "{n} projects", "de-DE": "{n} Projekte", "ja-JP": "{n} 件のプロジェクト", "zh-CN": "{n} 个项目" },
  "search.placeholder": { en: "search files, projects, reviews", "de-DE": "Dateien, Projekte, Reviews suchen", "ja-JP": "ファイル・プロジェクト・レビューを検索", "zh-CN": "搜索文件、项目、审阅" },

  "ob.title": { en: "Welcome to Cairn", "de-DE": "Willkommen bei Cairn", "ja-JP": "Cairn へようこそ", "zh-CN": "欢迎来到 Cairn" },
  "ob.sub": { en: "Choose the folder where your project lives.", "de-DE": "Wähle den Ordner, in dem dein Projekt liegt.", "ja-JP": "プロジェクトのあるフォルダーを選んでください。", "zh-CN": "选择项目所在的文件夹。" },
  "ob.sub2": { en: "Cairn keeps it available on your other computers automatically.", "de-DE": "Cairn hält ihn auf deinen anderen Rechnern automatisch verfügbar.", "ja-JP": "Cairn が自動的に他のパソコンでも使えるようにします。", "zh-CN": "Cairn 会自动让它在你其他电脑上也可用。" },
  "ob.hint": { en: "Working on a less powerful computer? Cairn can use lighter editing copies automatically.", "de-DE": "Arbeitest du auf einem schwächeren Rechner? Cairn kann automatisch leichtere Schnittkopien verwenden.", "ja-JP": "性能の低いパソコンで作業していますか？Cairn は自動的に軽い編集用コピーを使えます。", "zh-CN": "在性能较弱的电脑上工作？Cairn 可以自动使用更轻量的编辑副本。" },
  "ob.continue": { en: "Continue", "de-DE": "Weiter", "ja-JP": "続ける", "zh-CN": "继续" },
  "ob.daemonDown": { en: "Cairn isn't running", "de-DE": "Cairn läuft nicht", "ja-JP": "Cairn が起動していません", "zh-CN": "Cairn 未运行" },
  "ob.daemonDownSub": { en: "We're trying to restart it.", "de-DE": "Wir versuchen einen Neustart.", "ja-JP": "再起動を試みています。", "zh-CN": "正在尝试重启。" },

  "btn.add": { en: "Add project", "de-DE": "Projekt hinzufügen", "ja-JP": "プロジェクト追加", "zh-CN": "添加项目" },
  "btn.attach": { en: "Add folder", "de-DE": "Ordner hinzufügen", "ja-JP": "フォルダーを追加", "zh-CN": "添加文件夹" },
  "btn.addWs": { en: "Add project folder", "de-DE": "Projektordner hinzufügen", "ja-JP": "プロジェクトフォルダーを追加", "zh-CN": "添加项目文件夹" },
  "btn.copy": { en: "copy", "de-DE": "kopieren", "ja-JP": "コピー", "zh-CN": "复制" },
  "btn.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "btn.snapshot": { en: "Save a version", "de-DE": "Version speichern", "ja-JP": "バージョンを保存", "zh-CN": "保存版本" },
  "btn.restore": { en: "Restore", "de-DE": "Wiederherstellen", "ja-JP": "復元", "zh-CN": "恢复" },
  "btn.detach": { en: "Remove", "de-DE": "Entfernen", "ja-JP": "削除", "zh-CN": "移除" },
  "btn.pin": { en: "Keep on this computer", "de-DE": "Auf diesem Rechner behalten", "ja-JP": "このパソコンに保持", "zh-CN": "保留在此电脑" },
  "btn.unpin": { en: "Stop keeping it", "de-DE": "Nicht mehr behalten", "ja-JP": "保持をやめる", "zh-CN": "不再保留" },
  "btn.recall": { en: "Make available", "de-DE": "Verfügbar machen", "ja-JP": "利用可能にする", "zh-CN": "设为可用" },

  "panel.add": { en: "Add a project", "de-DE": "Ein Projekt hinzufügen", "ja-JP": "プロジェクトを追加", "zh-CN": "添加项目" },
  "panel.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "panel.makeAvail": { en: "Make files available", "de-DE": "Dateien verfügbar machen", "ja-JP": "ファイルを利用可能にする", "zh-CN": "让文件可用" },
  "panel.recall": { en: "Make files available", "de-DE": "Dateien verfügbar machen", "ja-JP": "ファイルを利用可能にする", "zh-CN": "让文件可用" },
  "field.root": { en: "Project folder", "de-DE": "Projektordner", "ja-JP": "プロジェクトフォルダー", "zh-CN": "项目文件夹" },
  "field.project": { en: "Project id", "de-DE": "Projekt-ID", "ja-JP": "プロジェクト ID", "zh-CN": "项目 ID" },
  "field.optional": { en: "optional", "de-DE": "optional", "ja-JP": "任意", "zh-CN": "可选" },
  "or.cli": { en: "or from the terminal", "de-DE": "oder im Terminal", "ja-JP": "またはターミナルから", "zh-CN": "或从终端" },
  "attach.root": { en: "/path/to/project", "de-DE": "/pfad/zum/projekt", "ja-JP": "/path/to/project", "zh-CN": "/path/to/project" },
  "attach.project": { en: "project id (optional)", "de-DE": "Projekt-ID (optional)", "ja-JP": "プロジェクト ID（任意）", "zh-CN": "项目 ID（可选）" },
  "versions.label": { en: "label (e.g. before color pass)", "de-DE": "Beschriftung (z.B. vor dem Colorgrading)", "ja-JP": "ラベル（例：カラーパスの前）", "zh-CN": "标签（如：调色前）" },
  "versions.unlabeled": { en: "untitled version", "de-DE": "unbenannte Version", "ja-JP": "無題のバージョン", "zh-CN": "未命名版本" },
  "recall.path": { en: "one file (optional, empty = whole project)", "de-DE": "eine Datei (optional, leer = ganzes Projekt)", "ja-JP": "1 ファイル（任意、空ならプロジェクト全体）", "zh-CN": "单个文件（可选，留空 = 整个项目）" },

  "dash.title": { en: "Your projects, in sync", "de-DE": "Deine Projekte, synchron", "ja-JP": "プロジェクトを同期したまま", "zh-CN": "项目随时同步" },
  "dash.sub": { en: "{n} project(s) · {files} files · {synced} ready", "de-DE": "{n} Projekt(e) · {files} Dateien · {synced} bereit", "ja-JP": "{n} 件のプロジェクト · {files} ファイル · {synced} 件準備完了", "zh-CN": "{n} 个项目 · {files} 个文件 · {synced} 个就绪" },
  "dash.subEmpty": { en: "Add your first project folder to start.", "de-DE": "Füge deinen ersten Projektordner hinzu.", "ja-JP": "最初のプロジェクトフォルダーを追加しましょう。", "zh-CN": "添加第一个项目文件夹即可开始。" },

  "card.assets": { en: "Recent files", "de-DE": "Neueste Dateien", "ja-JP": "最近のファイル", "zh-CN": "最近文件" },
  "card.sessions": { en: "Recent activity", "de-DE": "Letzte Aktivität", "ja-JP": "最近のアクティビティ", "zh-CN": "最近动态" },
  "note.assetsRecent": { en: "The newest files in your project. Use the row menu to keep a file on this computer.", "de-DE": "Die neuesten Dateien in deinem Projekt. Über das Zeilenmenü behältst du eine Datei auf diesem Rechner.", "ja-JP": "プロジェクトの最新ファイル。行のメニューから、このパソコンに保持できます。", "zh-CN": "项目中最新的文件。使用行菜单可将文件保留在这台电脑上。" },
  "label.recentSession": { en: "Recent project", "de-DE": "Letztes Projekt", "ja-JP": "最近のプロジェクト", "zh-CN": "最近的项目" },
  "label.activeProjects": { en: "Recent active projects", "de-DE": "Kürzlich aktive Projekte", "ja-JP": "最近アクティブなプロジェクト", "zh-CN": "最近活跃的项目" },
  "label.actions": { en: "Latest actions", "de-DE": "Letzte Aktionen", "ja-JP": "最新の操作", "zh-CN": "最新操作" },
  "label.activity": { en: "User activity", "de-DE": "Aktivität", "ja-JP": "ユーザーアクティビティ", "zh-CN": "用户活动" },
  "rs.summary": { en: "{synced}/{files} files ready", "de-DE": "{synced}/{files} Dateien bereit", "ja-JP": "{files} 件中 {synced} 件準備完了", "zh-CN": "{synced}/{files} 个文件就绪" },
  "act.saved": { en: "saving", "de-DE": "speichert", "ja-JP": "保存中", "zh-CN": "保存中" },
  "act.synced": { en: "ready", "de-DE": "bereit", "ja-JP": "準備完了", "zh-CN": "就绪" },
  "act.renamed": { en: "renamed", "de-DE": "umbenannt", "ja-JP": "名前変更", "zh-CN": "已重命名" },
  "act.deleted": { en: "deleted", "de-DE": "gelöscht", "ja-JP": "削除", "zh-CN": "已删除" },
  "act.pinned": { en: "kept on this computer", "de-DE": "auf diesem Rechner behalten", "ja-JP": "このパソコンに保持", "zh-CN": "已保留在此电脑" },
  "act.unpinned": { en: "no longer kept here", "de-DE": "nicht mehr behalten", "ja-JP": "保持をやめました", "zh-CN": "已取消保留" },
  "act.opened": { en: "opened in the editing app", "de-DE": "in der Editing-App geöffnet", "ja-JP": "編集アプリで開きました", "zh-CN": "已在剪辑软件中打开" },

  "chart.note": { en: "bytes copied per day, last 7 days", "de-DE": "kopierte Bytes pro Tag, letzte 7 Tage", "ja-JP": "1 日あたりのコピー量（過去 7 日）", "zh-CN": "每天复制的字节数（近 7 天）" },
  "chart.empty": { en: "No copies in the last 7 days.", "de-DE": "Keine Kopien in den letzten 7 Tagen.", "ja-JP": "過去 7 日間のコピーはありません。", "zh-CN": "最近 7 天没有复制记录。" },
  "chart.a11y": { en: "daily activity chart, peak {peak}", "de-DE": "Aktivitätsdiagramm, Spitze {peak}", "ja-JP": "日次アクティビティチャート、最大 {peak}", "zh-CN": "每日活动图，峰值 {peak}" },
  "presence.live": { en: "{n} editing right now", "de-DE": "{n} arbeiten gerade", "ja-JP": "{n} 人が編集中", "zh-CN": "{n} 人正在编辑" },

  "review.label": { en: "Review", "de-DE": "Review", "ja-JP": "レビュー", "zh-CN": "审阅" },
  "review.notes": { en: "{n} open notes", "de-DE": "{n} offene Notizen", "ja-JP": "未解決のメモ {n} 件", "zh-CN": "{n} 条未处理备注" },

  "set.system": { en: "System", "de-DE": "System", "ja-JP": "システム", "zh-CN": "系统" },
  "set.daemon": { en: "Cairn version", "de-DE": "Cairn-Version", "ja-JP": "Cairn バージョン", "zh-CN": "Cairn 版本" },
  "set.proto": { en: "protocol", "de-DE": "Protokoll", "ja-JP": "プロトコル", "zh-CN": "协议" },
  "set.uptime": { en: "uptime", "de-DE": "Laufzeit", "ja-JP": "稼働時間", "zh-CN": "运行时长" },
  "set.node": { en: "this computer", "de-DE": "dieser Rechner", "ja-JP": "このパソコン", "zh-CN": "这台电脑" },
  "set.update": { en: "update", "de-DE": "Update", "ja-JP": "アップデート", "zh-CN": "更新" },
  "set.updateOk": { en: "up to date", "de-DE": "auf dem neuesten Stand", "ja-JP": "最新です", "zh-CN": "已是最新" },
  "set.sysAdv": { en: "System details", "de-DE": "Systemdetails", "ja-JP": "システム詳細", "zh-CN": "系统详情" },
  "set.doctor": { en: "System health (doctor)", "de-DE": "Systemzustand (Doctor)", "ja-JP": "システム状態（doctor）", "zh-CN": "系统健康（doctor）" },
  "set.flags": { en: "Feature flags", "de-DE": "Feature-Flags", "ja-JP": "機能フラグ", "zh-CN": "功能开关" },
  "set.storage": { en: "Storage", "de-DE": "Speicher", "ja-JP": "ストレージ", "zh-CN": "存储" },
  "set.paths": { en: "Projects & paths", "de-DE": "Projekte & Pfade", "ja-JP": "プロジェクトとパス", "zh-CN": "项目与路径" },
  "set.appearance": { en: "Appearance", "de-DE": "Erscheinungsbild", "ja-JP": "外観", "zh-CN": "外观" },
  "set.quota": { en: "Storage used", "de-DE": "Belegter Speicher", "ja-JP": "使用中の容量", "zh-CN": "已用空间" },
  "set.storageAdv": { en: "Storage details", "de-DE": "Speicherdetails", "ja-JP": "ストレージ詳細", "zh-CN": "存储详情" },
  "set.themeLight": { en: "Light", "de-DE": "Hell", "ja-JP": "ライト", "zh-CN": "浅色" },
  "set.themeDark": { en: "Dark", "de-DE": "Dunkel", "ja-JP": "ダーク", "zh-CN": "深色" },
  "set.themeSystem": { en: "System", "de-DE": "System", "ja-JP": "システム", "zh-CN": "跟随系统" },
  "note.theme": { en: "Dark mode follows your choice here; the top bar toggle is the shortcut.", "de-DE": "Der Dunkelmodus folgt dieser Wahl; der Kippschalter oben ist der Kurzweg.", "ja-JP": "ダークモードはここでの選択に従います。上部のトグルがショートカットです。", "zh-CN": "深色模式跟随此处选择；顶栏开关是快捷方式。" },
  "stat.chunks": { en: "pieces of files stored here", "de-DE": "hier gespeicherte Dateistücke", "ja-JP": "保存済みのファイル断片", "zh-CN": "已存储的文件分片" },
  "stat.cached": { en: "space used by the cache", "de-DE": "vom Cache belegter Speicher", "ja-JP": "キャッシュの使用量", "zh-CN": "缓存占用空间" },
  "stat.pinned": { en: "files kept on this computer", "de-DE": "auf diesem Rechner behaltene Dateien", "ja-JP": "このパソコンに保持中のファイル", "zh-CN": "保留在此电脑的文件" },
  "note.locks": { en: "When someone opens a project file, Cairn marks it so you never overwrite each other's work.", "de-DE": "Wenn jemand eine Projektdatei öffnet, markiert Cairn sie, damit sich niemand gegenseitig die Arbeit überschreibt.", "ja-JP": "誰かがプロジェクトファイルを開くと、Cairn が印を付け、作業の上書きを防ぎます。", "zh-CN": "有人打开项目文件时，Cairn 会做好标记，避免互相覆盖工作。" },
  "note.versions": { en: "Saved versions are restore points. Cairn creates them automatically - and you can save one before any big change.", "de-DE": "Gespeicherte Versionen sind Wiederherstellungspunkte. Cairn erzeugt sie automatisch - und du kannst vor großen Änderungen eine speichern.", "ja-JP": "保存したバージョンは復元ポイントです。Cairn が自動的に作成し、大きな変更の前に手動でも保存できます。", "zh-CN": "已保存的版本就是还原点。Cairn 会自动创建 - 你也可以在大改动前手动保存一个。" },
  "note.makeAvail": { en: "Copy files from your other computers onto this one. Leave the box empty for the whole project.", "de-DE": "Dateien von deinen anderen Rechnern auf diesen kopieren. Leer lassen für das ganze Projekt.", "ja-JP": "他のパソコンからこのパソコンへファイルをコピーします。空欄ならプロジェクト全体。", "zh-CN": "把文件从其他电脑复制到这台。留空则处理整个项目。" },
  "note.recall": { en: "Copy files from your other computers onto this one. Leave the box empty for the whole project.", "de-DE": "Dateien von deinen anderen Rechnern auf diesen kopieren. Leer lassen für das ganze Projekt.", "ja-JP": "他のパソコンからこのパソコンへファイルをコピーします。空欄ならプロジェクト全体。", "zh-CN": "把文件从其他电脑复制到这台。留空则处理整个项目。" },
  "note.storage": { en: "Cairn keeps the files you work with on this computer. Files marked \"Keep on this computer\" are never removed automatically.", "de-DE": "Cairn behält die Dateien, mit denen du arbeitest, auf diesem Rechner. Als \"Auf diesem Rechner behalten\" markierte Dateien werden nie automatisch entfernt.", "ja-JP": "Cairn は作業中のファイルをこのパソコンに保持します。「このパソコンに保持」を付けたファイルは自動的に削除されません。", "zh-CN": "Cairn 会把你正在使用的文件保留在这台电脑上。标记为「保留在此电脑」的文件不会被自动清理。" },
  "note.live": { en: "Live presence is on: you see teammates scrubbing in the timeline (this device only).", "de-DE": "Live-Präsenz ist an: Du siehst Teammates in der Timeline scrubben (nur dieses Gerät).", "ja-JP": "ライブプレゼンスが有効：タイムライン上のチームメイトの動きが見えます（このデバイスのみ）。", "zh-CN": "实时在线已开启：可以看到队友在时间线上的位置（仅本设备）。" },
  "live.off": { en: "Live presence is off on this device. Turn on the live_presence flag to see teammates scrubbing in the timeline.", "de-DE": "Live-Präsenz ist auf diesem Gerät aus. Mit dem live_presence-Flag siehst du Teammates in der Timeline.", "ja-JP": "このデバイスではライブプレゼンスがオフです。live_presence フラグを有効にすると表示されます。", "zh-CN": "本设备已关闭实时在线。开启 live_presence 开关后可查看队友的位置。" },
  "quota.warn": { en: "This drive is almost full. Free up space to keep everything running.", "de-DE": "Dieses Laufwerk ist fast voll. Schaffe Platz, damit alles weiterläuft.", "ja-JP": "このドライブはほぼ満杯です。空き容量を確保してください。", "zh-CN": "此磁盘即将存满。请清理空间以保证正常运行。" },

  "files.filter": { en: "search this project", "de-DE": "dieses Projekt durchsuchen", "ja-JP": "このプロジェクトを検索", "zh-CN": "搜索此项目" },
  "files.summary": { en: "{files} files · {synced} ready · {syncing} working · {conflict} need attention", "de-DE": "{files} Dateien · {synced} bereit · {syncing} beschäftigt · {conflict} brauchen Aufmerksamkeit", "ja-JP": "{files} 件中 準備完了 {synced} · 処理中 {syncing} · 要対応 {conflict}", "zh-CN": "{files} 个文件 · {synced} 个就绪 · {syncing} 个处理中 · {conflict} 个需处理" },
  "files.synced": { en: "ready", "de-DE": "bereit", "ja-JP": "準備完了", "zh-CN": "就绪" },
  "files.syncing": { en: "Working…", "de-DE": "Beschäftigt…", "ja-JP": "処理中…", "zh-CN": "处理中…" },
  "files.conflict": { en: "Needs attention", "de-DE": "Braucht Aufmerksamkeit", "ja-JP": "要対応", "zh-CN": "需要处理" },
  "files.onlineOnly": { en: "Online only", "de-DE": "Nur online", "ja-JP": "オンラインのみ", "zh-CN": "仅在线" },
  "files.placeholder": { en: "Online only", "de-DE": "Nur online", "ja-JP": "オンラインのみ", "zh-CN": "仅在线" },
  "files.pinnedA11y": { en: "kept on this computer", "de-DE": "auf diesem Rechner behalten", "ja-JP": "このパソコンに保持中", "zh-CN": "已保留在此电脑" },
  "files.mergeAvailable": { en: "Merge available", "de-DE": "Zusammenführen möglich", "ja-JP": "マージできます", "zh-CN": "可以合并" },
  "files.mergeAccept": { en: "Merge", "de-DE": "Zusammenführen", "ja-JP": "マージ", "zh-CN": "合并" },
  "files.mergeDecline": { en: "Keep both", "de-DE": "Beide behalten", "ja-JP": "両方を保持", "zh-CN": "保留两者" },
  "files.error": { en: "Cairn isn't answering - the list fills in the moment it responds.", "de-DE": "Cairn antwortet gerade nicht - die Liste erscheint, sobald es antwortet.", "ja-JP": "Cairn が応答していません - 応答次第、一覧が表示されます。", "zh-CN": "Cairn 暂时没有响应 - 恢复后列表会立即显示。" },

  "th.file": { en: "file", "de-DE": "Datei", "ja-JP": "ファイル", "zh-CN": "文件" },
  "th.size": { en: "size", "de-DE": "Größe", "ja-JP": "サイズ", "zh-CN": "大小" },
  "th.sync": { en: "status", "de-DE": "Status", "ja-JP": "状態", "zh-CN": "状态" },
  "th.actions": { en: "actions", "de-DE": "Aktionen", "ja-JP": "操作", "zh-CN": "操作" },
  "th.status": { en: "status", "de-DE": "Status", "ja-JP": "状態", "zh-CN": "状态" },
  "th.label": { en: "label", "de-DE": "Beschriftung", "ja-JP": "ラベル", "zh-CN": "标签" },
  "th.author": { en: "author", "de-DE": "Autor", "ja-JP": "作成者", "zh-CN": "作者" },
  "th.member": { en: "member", "de-DE": "Mitglied", "ja-JP": "メンバー", "zh-CN": "成员" },
  "th.role": { en: "role", "de-DE": "Rolle", "ja-JP": "役割", "zh-CN": "角色" },
  "th.pinned": { en: "kept on this computer", "de-DE": "auf diesem Rechner behalten", "ja-JP": "このパソコンに保持中", "zh-CN": "已保留在此电脑" },

  "card.team": { en: "People & computers", "de-DE": "Personen & Rechner", "ja-JP": "メンバーとパソコン", "zh-CN": "成员与电脑" },
  "card.locks": { en: "Who's editing", "de-DE": "Wer bearbeitet", "ja-JP": "編集中の人", "zh-CN": "谁在编辑" },
  "people.lede": { en: "Who shares this project, and what each computer is doing right now.", "de-DE": "Wer dieses Projekt teilt und was jeder Rechner gerade tut.", "ja-JP": "このプロジェクトを共有しているメンバーと、各パソコンの現在の状態。", "zh-CN": "谁在共享这个项目，每台电脑正在做什么。" },
  "people.recordsTitle": { en: "Membership changes", "de-DE": "Änderungen im Team", "ja-JP": "メンバーの変更履歴", "zh-CN": "成员变动" },
  "people.recordsNote": { en: "Roster changes sync to every computer. These are the latest ones this computer knows about.", "de-DE": "Team-Änderungen werden mit allen Rechnern synchronisiert. Das sind die neuesten, die dieser Rechner kennt.", "ja-JP": "メンバーの変更はすべてのパソコンに同期されます。これはこのパソコンが知っている最新のものです。", "zh-CN": "成员变动会同步到每台电脑。这是这台电脑已知的最新变动。" },
  "people.recordsNone": { en: "No membership changes yet.", "de-DE": "Noch keine Team-Änderungen.", "ja-JP": "メンバーの変更はまだありません。", "zh-CN": "暂无成员变动。" },
  "people.recordJoined": { en: "joined", "de-DE": "ist beigetreten", "ja-JP": "が参加しました", "zh-CN": "已加入" },
  "people.recordLeft": { en: "was removed", "de-DE": "wurde entfernt", "ja-JP": "が削除されました", "zh-CN": "已被移除" },
  "lock.editingNow": { en: "Editing now", "de-DE": "Bearbeitet gerade", "ja-JP": "編集中", "zh-CN": "正在编辑" },
  "lock.earlier": { en: "Last edited earlier", "de-DE": "Vorher bearbeitet", "ja-JP": "以前に編集", "zh-CN": "此前编辑过" },

  "empty.projects": { en: "No projects yet - add your first folder.", "de-DE": "Noch keine Projekte - füge deinen ersten Ordner hinzu.", "ja-JP": "プロジェクトはまだありません - 最初のフォルダーを追加しましょう。", "zh-CN": "还没有项目 - 添加第一个文件夹。" },
  "empty.files": { en: "No files yet - add a project folder and drop media in.", "de-DE": "Noch keine Dateien - füge einen Projektordner hinzu und lege Medien hinein.", "ja-JP": "ファイルはまだありません - プロジェクトフォルダーを追加してメディアを入れてください。", "zh-CN": "还没有文件 - 添加项目文件夹并放入素材。" },
  "empty.activity": { en: "No actions yet.", "de-DE": "Noch keine Aktionen.", "ja-JP": "操作はまだありません。", "zh-CN": "暂无操作记录。" },
  "empty.locks": { en: "Nobody has a file open right now.", "de-DE": "Gerade hat niemand eine Datei offen.", "ja-JP": "現在ファイルを開いている人はいません。", "zh-CN": "现在没有人打开文件。" },
  "empty.versions": { en: "No versions yet - save one after the first sync.", "de-DE": "Noch keine Versionen - speichere eine nach dem ersten Abgleich.", "ja-JP": "バージョンはまだありません - 最初の同期後に保存しましょう。", "zh-CN": "还没有版本 - 首次同步后保存一个。" },
  "empty.makeAvail": { en: "Nothing is being copied right now.", "de-DE": "Gerade wird nichts kopiert.", "ja-JP": "現在コピー中のものはありません。", "zh-CN": "当前没有正在复制的内容。" },
  "empty.recall": { en: "Nothing is being copied right now.", "de-DE": "Gerade wird nichts kopiert.", "ja-JP": "現在コピー中のものはありません。", "zh-CN": "当前没有正在复制的内容。" },
  "empty.team": { en: "No one else is on this project yet - invite someone with a join code (More → Connect).", "de-DE": "Noch ist niemand sonst auf diesem Projekt - lade jemanden mit einem Einladungscode ein (Mehr → Verbinden).", "ja-JP": "このプロジェクトにはまだ誰もいません - 招待コードで招待しましょう（その他 → 接続）。", "zh-CN": "此项目还没有其他成员 - 用邀请码邀请（更多 → 连接）。" },

  "team.myRole": { en: "Your role", "de-DE": "Deine Rolle", "ja-JP": "あなたの役割", "zh-CN": "你的角色" },
  "team.invite": { en: "Invite code", "de-DE": "Einladungscode", "ja-JP": "招待コード", "zh-CN": "邀请码" },
  "team.audit": { en: "Recent permissions", "de-DE": "Letzte Berechtigungen", "ja-JP": "最近の権限", "zh-CN": "最近权限" },
  "team.allowed": { en: "allowed", "de-DE": "erlaubt", "ja-JP": "許可", "zh-CN": "允许" },
  "team.denied": { en: "denied", "de-DE": "verweigert", "ja-JP": "拒否", "zh-CN": "拒绝" },
  "team.you": { en: "you", "de-DE": "du", "ja-JP": "あなた", "zh-CN": "你" },

  "chip.ok": { en: "Up to date", "de-DE": "Aktuell", "ja-JP": "最新", "zh-CN": "已是最新" },
  "chip.warn": { en: "Working…", "de-DE": "Beschäftigt…", "ja-JP": "処理中…", "zh-CN": "处理中…" },
  "chip.bad": { en: "Cairn isn't running", "de-DE": "Cairn läuft nicht", "ja-JP": "Cairn が起動していません", "zh-CN": "Cairn 未运行" },
  "chip.error": { en: "Needs attention", "de-DE": "Braucht Aufmerksamkeit", "ja-JP": "要対応", "zh-CN": "需要处理" },
  "chip.retry": { en: "Needs attention", "de-DE": "Braucht Aufmerksamkeit", "ja-JP": "要対応", "zh-CN": "需要处理" },
  "chip.update": { en: "update available", "de-DE": "Update verfügbar", "ja-JP": "アップデートあり", "zh-CN": "有可用更新" },
  "chip.updateFailed": { en: "update check failed", "de-DE": "Update-Prüfung fehlgeschlagen", "ja-JP": "アップデート確認に失敗", "zh-CN": "检查更新失败" },
  "node.online": { en: "online", "de-DE": "online", "ja-JP": "オンライン", "zh-CN": "在线" },
  "node.offline": { en: "offline", "de-DE": "offline", "ja-JP": "オフライン", "zh-CN": "离线" },

  "help.title": { en: "Shortcuts & cheatsheet", "de-DE": "Tastenkürzel & Spickzettel", "ja-JP": "ショートカットと早見表", "zh-CN": "快捷键与速查表" },
  "help.keys": { en: "Keys", "de-DE": "Tasten", "ja-JP": "キー", "zh-CN": "按键" },
  "help.cli": { en: "CLI cheatsheet", "de-DE": "CLI-Spickzettel", "ja-JP": "CLI 早見表", "zh-CN": "命令行速查" },
  "help.states": { en: "What the dot means", "de-DE": "Was der Punkt bedeutet", "ja-JP": "ドットの意味", "zh-CN": "圆点的含义" },
  "help.search": { en: "focus search", "de-DE": "Suche fokussieren", "ja-JP": "検索へフォーカス", "zh-CN": "聚焦搜索" },
  "help.help": { en: "toggle this panel", "de-DE": "diese Tafel umschalten", "ja-JP": "このパネルを切替", "zh-CN": "打开/关闭此面板" },
  "help.goDash": { en: "go to Home", "de-DE": "zu Start", "ja-JP": "ホームへ", "zh-CN": "前往主页" },
  "help.goFiles": { en: "go to files", "de-DE": "zu Dateien", "ja-JP": "ファイルへ", "zh-CN": "前往文件" },
  "help.goPeople": { en: "go to People", "de-DE": "zu Personen", "ja-JP": "メンバーへ", "zh-CN": "前往成员" },
  "help.goSettings": { en: "go to settings", "de-DE": "zu Einstellungen", "ja-JP": "設定へ", "zh-CN": "前往设置" },
  "help.goHowto": { en: "go to the how-to guide", "de-DE": "zur Anleitung", "ja-JP": "使い方へ", "zh-CN": "前往使用指南" },
  "help.esc": { en: "close panels and overlays", "de-DE": "Panels schließen", "ja-JP": "パネルを閉じる", "zh-CN": "关闭面板" },
  "help.dotOk": { en: "everything ready - safe to edit", "de-DE": "alles bereit - ruhig editieren", "ja-JP": "すべて準備完了 - 編集OK", "zh-CN": "一切就绪 - 可以放心编辑" },
  "help.dotWarn": { en: "copying in the background", "de-DE": "kopiert im Hintergrund", "ja-JP": "バックグラウンドでコピー中", "zh-CN": "正在后台复制" },
  "help.dotBad": { en: "needs attention - see Home", "de-DE": "braucht Aufmerksamkeit - siehe Start", "ja-JP": "要対応 - ホームを見て", "zh-CN": "需要处理 - 见主页" },

  "toast.attached": { en: "project folder added", "de-DE": "Projektordner hinzugefügt", "ja-JP": "プロジェクトフォルダーを追加しました", "zh-CN": "已添加项目文件夹" },
  "toast.detached": { en: "folder removed from Cairn - your files stayed on this computer", "de-DE": "Ordner aus Cairn entfernt - deine Dateien bleiben auf diesem Rechner", "ja-JP": "フォルダーを Cairn から取り外しました - ファイルはこのパソコンに残ります", "zh-CN": "已从 Cairn 移除文件夹 - 文件仍保留在这台电脑上" },
  "toast.pinned": { en: "Cairn will keep this file on this computer", "de-DE": "Cairn behält diese Datei auf diesem Rechner", "ja-JP": "Cairn はこのファイルをこのパソコンに保持します", "zh-CN": "Cairn 会将此文件保留在这台电脑上" },
  "toast.unpinned": { en: "Cairn can free this file's space again when needed", "de-DE": "Cairn kann den Platz dieser Datei bei Bedarf wieder freigeben", "ja-JP": "必要に応じて Cairn がこのファイルの容量を解放できます", "zh-CN": "需要时 Cairn 可以重新释放此文件占用的空间" },
  "toast.makeAvailStarted": { en: "Copying started - you can keep working", "de-DE": "Kopie gestartet - du kannst weiterarbeiten", "ja-JP": "コピーを開始しました - 作業を続けられます", "zh-CN": "已开始复制 - 你可以继续工作" },
  "toast.recallStarted": { en: "Copying started - you can keep working", "de-DE": "Kopie gestartet - du kannst weiterarbeiten", "ja-JP": "コピーを開始しました - 作業を続けられます", "zh-CN": "已开始复制 - 你可以继续工作" },
  "toast.mergeAccepted": { en: "Merged - both sets of changes are now one version", "de-DE": "Zusammengeführt - beide Änderungen sind jetzt eine Version", "ja-JP": "マージしました - 両方の変更が 1 つのバージョンになりました", "zh-CN": "已合并 - 两边改动现在合为一个版本" },
  "toast.mergeDeclined": { en: "Kept both versions - your copy is untouched", "de-DE": "Beide Versionen behalten - deine Kopie ist unberührt", "ja-JP": "両方のバージョンを保持しました - あなたのコピーはそのままです", "zh-CN": "已保留两个版本 - 你的副本未被改动" },
  "toast.mergeFail": { en: "Cairn couldn't merge these two versions - your files are untouched.", "de-DE": "Cairn konnte diese zwei Versionen nicht zusammenführen - deine Dateien sind unberührt.", "ja-JP": "この 2 つのバージョンをマージできませんでした - ファイルはそのままです。", "zh-CN": "无法合并这两个版本 - 你的文件未受影响。" },
  "toast.versionCreated": { en: "Version saved - it's a restore point", "de-DE": "Version gespeichert - sie ist ein Wiederherstellungspunkt", "ja-JP": "バージョンを保存しました - 復元ポイントとして使えます", "zh-CN": "已保存版本 - 可作为还原点" },
  /* the checkpoint-bearing toast is used only when the backend answered
     with checkpoint_version (merged 2-d backend); older daemons get the
     plain "restored" line, which never claims a checkpoint */
  "toast.restored": { en: "Restored {n} files ({b}). Every change made since that version is still in the version list.", "de-DE": "{n} Dateien wiederhergestellt ({b}). Alle Änderungen seit dieser Version bleiben in der Versionsliste.", "ja-JP": "{n} 件のファイルを復元しました（{b}）。このバージョン以降の変更はバージョン一覧に残ります。", "zh-CN": "已恢复 {n} 个文件（{b}）。该版本之后的所有改动仍保留在版本列表中。" },
  "toast.restoredCkpt": { en: "Restored {n} files ({b}). Cairn saved the current state first as version {v}.", "de-DE": "{n} Dateien wiederhergestellt ({b}). Cairn hat den aktuellen Stand zuerst als Version {v} gesichert.", "ja-JP": "{n} 件のファイルを復元しました（{b}）。Cairn は現在の状態を先にバージョン {v} として保存しました。", "zh-CN": "已恢复 {n} 个文件（{b}）。Cairn 已先将当前状态保存为版本 {v}。" },
  "toast.copied": { en: "copied", "de-DE": "kopiert", "ja-JP": "コピーしました", "zh-CN": "已复制" },
  "toast.denied": { en: "Cairn couldn't do that: {e}", "de-DE": "Cairn konnte das nicht ausführen: {e}", "ja-JP": "実行できませんでした: {e}", "zh-CN": "无法完成该操作：{e}" },
  "toast.failed": { en: "That didn't work: {e}", "de-DE": "Das hat nicht geklappt: {e}", "ja-JP": "うまくいきませんでした: {e}", "zh-CN": "操作失败：{e}" },
  "toast.sharedLink": { en: "project link copied - teammates with access can open it", "de-DE": "Projekt-Link kopiert - Teammates mit Zugang können ihn öffnen", "ja-JP": "プロジェクトリンクをコピーしました - アクセス権のあるチームメイトが開けます", "zh-CN": "项目链接已复制 - 有权限的队友可打开" },
  "toast.recallFirst": { en: "This file isn't on this computer yet - choose Make available first.", "de-DE": "Diese Datei ist noch nicht auf diesem Rechner - erst \"Verfügbar machen\" wählen.", "ja-JP": "このファイルはまだこのパソコンにありません - 先に「利用可能にする」を選んでください。", "zh-CN": "此文件还不在这台电脑上 - 请先选择「设为可用」。" },
  "confirm.detach": { en: "Remove {p} from Cairn?", "de-DE": "{p} aus Cairn entfernen?", "ja-JP": "{p} を Cairn から取り外しますか？", "zh-CN": "从 Cairn 移除 {p} 吗？" },
  "confirm.restore": { en: "Restore this version?", "de-DE": "Diese Version wiederherstellen?", "ja-JP": "このバージョンを復元しますか？", "zh-CN": "恢复此版本吗？" },

  /* confirm modal (review #38-#41): consequences first, honest labels */
  "confirm.detachTitle": { en: "Remove this folder from Cairn?", "de-DE": "Diesen Ordner aus Cairn entfernen?", "ja-JP": "このフォルダーを Cairn から取り外しますか？", "zh-CN": "从 Cairn 移除此文件夹？" },
  "confirm.detachBody": { en: "Your files stay on this computer. Cairn will stop syncing this folder with your other computers.", "de-DE": "Deine Dateien bleiben auf diesem Rechner. Cairn synchronisiert diesen Ordner nicht mehr mit deinen anderen Rechnern.", "ja-JP": "ファイルはこのパソコンに残ります。Cairn はこのフォルダーの同期を停止します。", "zh-CN": "文件会保留在这台电脑上。Cairn 将停止同步此文件夹。" },
  "confirm.restoreTitle": { en: "Restore this version?", "de-DE": "Diese Version wiederherstellen?", "ja-JP": "このバージョンを復元しますか？", "zh-CN": "恢复此版本吗？" },
  /* restore copy: honest about the safety net. The merged 2-d backend
     checkpoints the current state BEFORE restoring and answers
     checkpoint_version, so the dialog may say "the current state is
     saved first"; the success toast names the checkpoint version only
     when the response actually carried one (older daemons degrade). */
  "confirm.restoreBody": { en: "The project's files go back to how they were at \"{label}\". Work saved after this version stays in the version list. This project currently holds {n} files. Cairn saves the current state first, so you can undo this.", "de-DE": "Die Dateien des Projekts stehen wieder so wie bei \"{label}\". Später gespeicherte Arbeit bleibt in der Versionsliste. Das Projekt hat gerade {n} Dateien. Cairn sichert den aktuellen Stand zuerst, das lässt sich zurücknehmen.", "ja-JP": "プロジェクトのファイルを「{label}」の時点の状態に戻します。それ以降に保存した作業はバージョン一覧に残ります。現在のプロジェクトには {n} 件のファイルがあります。Cairn はまず現在の状態を保存するので、元に戻せます。", "zh-CN": "项目文件将回到「{label}」当时的状态。此后保存的工作仍保留在版本列表中。当前项目有 {n} 个文件。Cairn 会先保存当前状态，因此可以撤销。" },
  "confirm.restoreCkpt": { en: "Cairn saved the current state first as version {v}.", "de-DE": "Cairn hat den aktuellen Stand zuerst als Version {v} gesichert.", "ja-JP": "Cairn は現在の状態を先にバージョン {v} として保存しました。", "zh-CN": "Cairn 已先将当前状态保存为版本 {v}。" },
  "modal.cancel": { en: "Cancel", "de-DE": "Abbrechen", "ja-JP": "キャンセル", "zh-CN": "取消" },

  /* merge-offer dialog (CONTRACT-DEBT #1, human copy) */
  "mergeDlg.title": { en: "Two versions changed", "de-DE": "Zwei Versionen wurden geändert", "ja-JP": "2 つのバージョンが変更されました", "zh-CN": "两个版本都有改动" },
  "mergeDlg.body": { en: "Cairn found two versions that changed in different places. Merge puts both sets of changes together - nothing is lost.", "de-DE": "Cairn hat zwei Versionen gefunden, die sich an verschiedenen Stellen geändert haben. Zusammenführen bringt beide Änderungen zusammen - nichts geht verloren.", "ja-JP": "別々の場所で変更された 2 つのバージョンが見つかりました。マージすると両方の変更がまとまり、失われるものはありません。", "zh-CN": "Cairn 发现在不同位置各自改动的两个版本。合并会把两边改动合在一起 - 不会丢失任何内容。" },
  "mergeDlg.sideOriginal": { en: "The version everyone started from", "de-DE": "Die Version, von der alle ausgingen", "ja-JP": "全員の起点となったバージョン", "zh-CN": "大家共同的起始版本" },
  "mergeDlg.sideYours": { en: "Edits made on this computer", "de-DE": "Auf diesem Rechner gemachte Änderungen", "ja-JP": "このパソコンで行った編集", "zh-CN": "在这台电脑上做的修改" },
  "mergeDlg.sideTheirs": { en: "Edits that arrived from other computers", "de-DE": "Änderungen von anderen Rechnern", "ja-JP": "他のパソコンから届いた編集", "zh-CN": "从其他电脑传来的修改" },
  "mergeDlg.keepNote": { en: "Choosing Keep both keeps a separate copy of your version - nothing is deleted.", "de-DE": "Beide behalten behält eine separate Kopie deiner Version - nichts wird gelöscht.", "ja-JP": "「両方を保持」を選ぶとあなたのバージョンが別コピーとして残ります。削除はされません。", "zh-CN": "选择「保留两者」会将你的版本另存一份副本 - 不会删除任何内容。" },
  "mergeDlg.keepBoth": { en: "Keep both", "de-DE": "Beide behalten", "ja-JP": "両方を保持", "zh-CN": "保留两者" },
  "mergeDlg.merge": { en: "Merge", "de-DE": "Zusammenführen", "ja-JP": "マージ", "zh-CN": "合并" },

  /* proxy (editing copies) - contract: POST /api/v1/proxy/generate */
  "proxy.make": { en: "Make editing copy", "de-DE": "Schnittkopie erstellen", "ja-JP": "編集用コピーを作成", "zh-CN": "创建编辑副本" },
  "proxy.making": { en: "Making editing copy…", "de-DE": "Schnittkopie wird erstellt…", "ja-JP": "編集用コピーを作成中…", "zh-CN": "正在创建编辑副本…" },
  "proxy.ready": { en: "Editing copy ready", "de-DE": "Schnittkopie bereit", "ja-JP": "編集用コピー準備完了", "zh-CN": "编辑副本已就绪" },
  "proxy.started": { en: "Making a lighter editing copy - editing stays smooth on this computer.", "de-DE": "Eine leichtere Schnittkopie entsteht - das Editing bleibt auf diesem Rechner flüssig.", "ja-JP": "軽い編集用コピーを作成しています - このパソコンでも快適に編集できます。", "zh-CN": "正在创建更轻量的编辑副本 - 这台电脑上也能流畅剪辑。" },
  "proxy.startedRel": { en: "Making a lighter editing copy - it will appear at {p}.", "de-DE": "Eine leichtere Schnittkopie entsteht - sie erscheint unter {p}.", "ja-JP": "軽い編集用コピーを作成しています - {p} に作られます。", "zh-CN": "正在创建更轻量的编辑副本 - 将生成到 {p}。" },
  "proxy.exists": { en: "Cairn is already making the editing copy.", "de-DE": "Cairn erstellt die Schnittkopie bereits.", "ja-JP": "Cairn はすでに編集用コピーを作成中です。", "zh-CN": "Cairn 已在创建编辑副本。" },
  "proxy.fail": { en: "Cairn couldn't make the editing copy yet - try again in a moment.", "de-DE": "Cairn konnte die Schnittkopie noch nicht erstellen - gleich nochmal versuchen.", "ja-JP": "編集用コピーを作成できませんでした - しばらくしてからもう一度。", "zh-CN": "暂时无法创建编辑副本 - 请稍后重试。" },

  /* busy labels (#43): buttons say what they are doing while disabled */
  "busy.publish": { en: "Publishing…", "de-DE": "Veröffentlichen…", "ja-JP": "公開中…", "zh-CN": "发布中…" },
  "busy.link": { en: "Generating…", "de-DE": "Wird erzeugt…", "ja-JP": "生成中…", "zh-CN": "生成中…" },
  "busy.restore": { en: "Restoring…", "de-DE": "Wird wiederhergestellt…", "ja-JP": "復元中…", "zh-CN": "恢复中…" },
  "busy.duplicate": { en: "Copying…", "de-DE": "Wird kopiert…", "ja-JP": "コピー中…", "zh-CN": "复制中…" },
  "busy.attach": { en: "Adding…", "de-DE": "Wird hinzugefügt…", "ja-JP": "追加中…", "zh-CN": "添加中…" },
  "busy.avail": { en: "Preparing…", "de-DE": "Wird vorbereitet…", "ja-JP": "準備中…", "zh-CN": "准备中…" },
  "busy.merge": { en: "Merging…", "de-DE": "Wird zusammengeführt…", "ja-JP": "マージ中…", "zh-CN": "合并中…" },
  "busy.working": { en: "Working…", "de-DE": "Beschäftigt…", "ja-JP": "処理中…", "zh-CN": "处理中…" },
  "busy.saving": { en: "Saving…", "de-DE": "Wird gespeichert…", "ja-JP": "保存中…", "zh-CN": "保存中…" },
  "busy.generatingCode": { en: "Generating…", "de-DE": "Wird erzeugt…", "ja-JP": "生成中…", "zh-CN": "生成中…" },
  "busy.joining": { en: "Joining…", "de-DE": "Beitreten…", "ja-JP": "参加中…", "zh-CN": "加入中…" },

  /* relative time (was hardcoded English) */
  "rel.now": { en: "just now", "de-DE": "gerade eben", "ja-JP": "たった今", "zh-CN": "刚刚" },
  "rel.min": { en: "{n}m ago", "de-DE": "vor {n} Min.", "ja-JP": "{n} 分前", "zh-CN": "{n} 分钟前" },
  "rel.hour": { en: "{n}h ago", "de-DE": "vor {n} Std.", "ja-JP": "{n} 時間前", "zh-CN": "{n} 小时前" },
  "rel.day": { en: "{n}d ago", "de-DE": "vor {n} Tagen", "ja-JP": "{n} 日前", "zh-CN": "{n} 天前" },

  "view.connect": { en: "Connect", "de-DE": "Verbinden", "ja-JP": "接続", "zh-CN": "连接" },
  "view.review": { en: "Review", "de-DE": "Review", "ja-JP": "レビュー", "zh-CN": "审阅" },
  "view.merge": { en: "Merge", "de-DE": "Zusammenführen", "ja-JP": "マージ", "zh-CN": "合并" },
  "connect.lede": { en: "Before your computers can share files, connect them with a join code. Generate one below, share it, done.", "de-DE": "Bevor deine Rechner Dateien teilen können, verbinde sie mit einem Einladungscode. Unten erzeugen, teilen, fertig.", "ja-JP": "パソコン同士でファイルを共有するには、招待コードで接続します。下で生成して共有するだけです。", "zh-CN": "电脑之间要共享文件，先用邀请码连接。在下面生成、分享即可。" },
  "connect.codeTitle": { en: "Your join code", "de-DE": "Dein Einladungscode", "ja-JP": "あなたの招待コード", "zh-CN": "你的邀请码" },
  "connect.codeHint": { en: "Click Generate code, then share the code. Your teammate pastes it under Join below.", "de-DE": "Klicke auf Code erzeugen und teile den Code. Dein Teammate fügt ihn unten unter Beitreten ein.", "ja-JP": "「コードを生成」をクリックしてコードを共有。チームメイトは下の「参加」に貼り付けます。", "zh-CN": "点击「生成邀请码」并分享。队友在下方「加入」处粘贴。" },
  "connect.hintCode": { en: "This code is ready to share - it works until you generate a new one.", "de-DE": "Dieser Code ist bereit zum Teilen - er gilt, bis du einen neuen erzeugst.", "ja-JP": "このコードは共有可能です - 新しく生成するまで有効です。", "zh-CN": "此邀请码可以分享 - 生成新码前一直有效。" },
  "connect.hintNone": { en: "No code yet. Hit Generate code.", "de-DE": "Noch kein Code. Klicke auf Code erzeugen.", "ja-JP": "コードがまだありません。「コードを生成」を押してください。", "zh-CN": "还没有邀请码。请点击「生成邀请码」。" },
  "connect.generate": { en: "Generate code", "de-DE": "Code erzeugen", "ja-JP": "コードを生成", "zh-CN": "生成邀请码" },
  "connect.details": { en: "Connection details", "de-DE": "Verbindungsdetails", "ja-JP": "接続の詳細", "zh-CN": "连接详情" },
  "connect.signal": { en: "Signal", "de-DE": "Signal", "ja-JP": "シグナル", "zh-CN": "信号" },
  "connect.swarm": { en: "Swarm", "de-DE": "Swarm", "ja-JP": "スウォーム", "zh-CN": "群组" },
  "connect.online": { en: "online", "de-DE": "online", "ja-JP": "オンライン", "zh-CN": "在线" },
  "connect.offline": { en: "offline", "de-DE": "offline", "ja-JP": "オフライン", "zh-CN": "离线" },
  "connect.noSignal": { en: "not connected yet", "de-DE": "noch nicht verbunden", "ja-JP": "まだ未接続", "zh-CN": "尚未连接" },
  "connect.noSwarm": { en: "not connected yet", "de-DE": "noch nicht verbunden", "ja-JP": "まだ未接続", "zh-CN": "尚未连接" },
  "connect.joinTitle": { en: "Join another computer", "de-DE": "Einem anderen Rechner beitreten", "ja-JP": "別のパソコンに参加", "zh-CN": "加入其他电脑" },
  "connect.codePh": { en: "Paste join code", "de-DE": "Einladungscode einfügen", "ja-JP": "招待コードを貼り付け", "zh-CN": "粘贴邀请码" },
  "connect.join": { en: "Join", "de-DE": "Beitreten", "ja-JP": "参加", "zh-CN": "加入" },
  "connect.howTitle": { en: "How it connects", "de-DE": "Wie verbunden wird", "ja-JP": "接続のしくみ", "zh-CN": "连接方式" },
  "connect.how": { en: "Computers connect directly when possible - on the same network a big copy takes minutes, not hours. When a direct line can't be found, an encrypted relay carries the files.", "de-DE": "Rechner verbinden sich direkt, wenn möglich - im selben Netzwerk dauert eine große Kopie Minuten, nicht Stunden. Wenn keine direkte Verbindung gefunden wird, trägt ein verschlüsselter Relais die Dateien.", "ja-JP": "可能な限り直接接続します - 同じネットワークなら大きなコピーも数分。直接接続できない場合は暗号化された中継がファイルを運びます。", "zh-CN": "电脑会尽可能直连 - 同一网络下大文件复制只需几分钟。找不到直连路径时，会通过加密中继传输。" },
  "connect.noCode": { en: "No code to copy yet - generate one first.", "de-DE": "Noch kein Code zum Kopieren - erst einen erzeugen.", "ja-JP": "コピーできるコードがまだありません - 先に生成してください。", "zh-CN": "还没有可复制的邀请码 - 请先生成。" },
  "connect.copied": { en: "code copied - send it to your teammate", "de-DE": "Code kopiert - schicke ihn deinem Teammate", "ja-JP": "コードをコピーしました - チームメイトに送ってください", "zh-CN": "邀请码已复制 - 请发给队友" },
  "connect.generated": { en: "New code ready: {c}", "de-DE": "Neuer Code bereit: {c}", "ja-JP": "新しいコード: {c}", "zh-CN": "新邀请码已生成：{c}" },
  "connect.joinFirst": { en: "Paste a join code first.", "de-DE": "Zuerst einen Einladungscode einfügen.", "ja-JP": "先に招待コードを貼り付けてください。", "zh-CN": "请先粘贴邀请码。" },
  "connect.joined": { en: "Code accepted.", "de-DE": "Code angenommen.", "ja-JP": "コードを受け付けました。", "zh-CN": "邀请码已接受。" },
  "connect.joinFailed": { en: "That code didn't work: {e}", "de-DE": "Dieser Code hat nicht funktioniert: {e}", "ja-JP": "このコードは使えませんでした: {e}", "zh-CN": "该邀请码无效：{e}" },
  "connect.needProject": { en: "Add a project folder first.", "de-DE": "Füge zuerst einen Projektordner hinzu.", "ja-JP": "先にプロジェクトフォルダーを追加してください。", "zh-CN": "请先添加项目文件夹。" },

  "review.lede": { en: "Publish a cut, generate a link, share it. Your client opens it in a browser and leaves frame notes.", "de-DE": "Schnitt veröffentlichen, Link erzeugen, teilen. Dein Client öffnet ihn im Browser und hinterlässt Frame-Notizen.", "ja-JP": "カットを公開し、リンクを生成して共有。クライアントはブラウザーで開き、フレームにメモを残せます。", "zh-CN": "发布成片、生成链接、分享。客户在浏览器中打开即可在帧上留言。" },
  "review.versions": { en: "Versions", "de-DE": "Versionen", "ja-JP": "バージョン", "zh-CN": "版本" },
  "review.chooseMedia": { en: "Video to publish", "de-DE": "Zu veröffentlichendes Video", "ja-JP": "公開する動画", "zh-CN": "要发布的视频" },
  "review.chooseMediaA11y": { en: "Choose the video to publish", "de-DE": "Zu veröffentlichendes Video wählen", "ja-JP": "公開する動画を選択", "zh-CN": "选择要发布的视频" },
  "review.publish": { en: "Publish for review", "de-DE": "Zum Review veröffentlichen", "ja-JP": "レビュー用に公開", "zh-CN": "发布以供审阅" },
  "review.published": { en: "Published - version {n} is ready for a link", "de-DE": "Veröffentlicht - Version {n} ist bereit für einen Link", "ja-JP": "公開しました - バージョン {n} にリンクを付けられます", "zh-CN": "已发布 - 版本 {n} 可以生成链接了" },
  "review.noVersions": { en: "Nothing published yet - publish a cut above, then generate a link.", "de-DE": "Noch nichts veröffentlicht - oben einen Schnitt veröffentlichen, dann einen Link erzeugen.", "ja-JP": "まだ公開されていません - 上でカットを公開してからリンクを生成してください。", "zh-CN": "还没有发布 - 先在上方发布成片，再生成链接。" },
  "review.noMedia": { en: "No publishable video found - files that are online-only must be made available first.", "de-DE": "Kein veröffentlichbares Video gefunden - Nur-online-Dateien müssen erst verfügbar gemacht werden.", "ja-JP": "公開できる動画が見つかりません - オンラインのみのファイルは先に利用可能にしてください。", "zh-CN": "未找到可发布的视频 - 仅在线的文件需先设为可用。" },
  "review.untitled": { en: "Untitled cut", "de-DE": "Unbenannter Schnitt", "ja-JP": "無題のカット", "zh-CN": "未命名成片" },
  "review.vTitle": { en: "Version {n} · {label}", "de-DE": "Version {n} · {label}", "ja-JP": "バージョン {n} · {label}", "zh-CN": "版本 {n} · {label}" },
  "review.vDetails": { en: "{frames} frames · {fps} fps · {d}", "de-DE": "{frames} Frames · {fps} fps · {d}", "ja-JP": "{frames} フレーム · {fps} fps · {d}", "zh-CN": "{frames} 帧 · {fps} fps · {d}" },
  "review.linkTitle": { en: "Generate review link", "de-DE": "Review-Link erzeugen", "ja-JP": "レビューリンクを生成", "zh-CN": "生成审阅链接" },
  "review.noteLabel": { en: "Note", "de-DE": "Notiz", "ja-JP": "メモ", "zh-CN": "备注" },
  "review.notePh": { en: "Client A", "de-DE": "Kunde A", "ja-JP": "クライアント A", "zh-CN": "客户 A" },
  "review.roleLabel": { en: "Role", "de-DE": "Rolle", "ja-JP": "役割", "zh-CN": "角色" },
  "review.roleCommenter": { en: "Can comment", "de-DE": "Kann kommentieren", "ja-JP": "コメント可", "zh-CN": "可以评论" },
  "review.roleViewer": { en: "View only", "de-DE": "Nur ansehen", "ja-JP": "閲覧のみ", "zh-CN": "仅可查看" },
  "review.roleStudio": { en: "Studio (internal)", "de-DE": "Studio (intern)", "ja-JP": "スタジオ（内部）", "zh-CN": "工作室（内部）" },
  "review.expiresLabel": { en: "Expires", "de-DE": "Läuft ab", "ja-JP": "有効期限", "zh-CN": "有效期" },
  "review.ttl7": { en: "7 days", "de-DE": "7 Tage", "ja-JP": "7 日", "zh-CN": "7 天" },
  "review.ttl30": { en: "30 days", "de-DE": "30 Tage", "ja-JP": "30 日", "zh-CN": "30 天" },
  "review.ttl90": { en: "90 days", "de-DE": "90 Tage", "ja-JP": "90 日", "zh-CN": "90 天" },
  "review.linkCreate": { en: "Generate link", "de-DE": "Link erzeugen", "ja-JP": "リンクを生成", "zh-CN": "生成链接" },
  "review.linkReady": { en: "Link ready", "de-DE": "Link bereit", "ja-JP": "リンク準備完了", "zh-CN": "链接已就绪" },
  "review.linkExpiresOn": { en: "expires {d}", "de-DE": "läuft am {d} ab", "ja-JP": "{d} に失効", "zh-CN": "{d} 到期" },
  "review.linkNote": { en: "Anyone with the link can view this cut until it expires. Remove it below anytime.", "de-DE": "Jeder mit dem Link kann diesen Schnitt ansehen, bis er abläuft. Unten jederzeit entfernen.", "ja-JP": "リンクを知っている人は誰でも、失効までこのカットを閲覧できます。下でいつでも削除できます。", "zh-CN": "任何人拿到链接即可在到期前查看此成片。可随时在下方移除。" },
  "review.linkAnyone": { en: "Anyone with the link can view this cut.", "de-DE": "Jeder mit dem Link kann diesen Schnitt ansehen.", "ja-JP": "リンクを知っている人は誰でもこのカットを閲覧できます。", "zh-CN": "任何人拿到链接即可查看此成片。" },
  "review.linkCopy": { en: "Copy link", "de-DE": "Link kopieren", "ja-JP": "リンクをコピー", "zh-CN": "复制链接" },
  "review.selfHost": { en: "Self-hosting the review portal (advanced)", "de-DE": "Review-Portal selbst hosten (erweitert)", "ja-JP": "レビューポータルのセルフホスト（上級者向け）", "zh-CN": "自建审阅门户（高级）" },
  "review.frameNotes": { en: "Frame notes", "de-DE": "Frame-Notizen", "ja-JP": "フレームメモ", "zh-CN": "帧备注" },
  "review.expires": { en: "expires {d}", "de-DE": "läuft am {d} ab", "ja-JP": "{d} に失効", "zh-CN": "{d} 到期" },
  "review.revokedRemote": { en: "Revoked on another computer", "de-DE": "Auf einem anderen Rechner widerrufen", "ja-JP": "別のパソコンで失効済み", "zh-CN": "已在其他电脑上撤销" },
  "review.never": { en: "never expires", "de-DE": "läuft nie ab", "ja-JP": "失効しない", "zh-CN": "永不到期" },
  "review.expired": { en: "expired", "de-DE": "abgelaufen", "ja-JP": "失効済み", "zh-CN": "已到期" },
  "review.revoke": { en: "Remove link", "de-DE": "Link entfernen", "ja-JP": "リンクを削除", "zh-CN": "移除链接" },
  "review.revoked": { en: "Link removed - it stops working everywhere", "de-DE": "Link entfernt - er funktioniert nirgends mehr", "ja-JP": "リンクを削除しました - どこでも無効になります", "zh-CN": "链接已移除 - 在所有设备上立即失效" },

  "merge.lede": { en: "Two editors, one timeline. Cairn puts both versions together - every edit kept. When both of you changed the same place, a dialog offers the merge right in Files.", "de-DE": "Zwei Editoren, eine Timeline. Cairn bringt beide Versionen zusammen - kein Schnitt geht verloren. Haben beide dieselbe Stelle geändert, bietet ein Dialog den Merge direkt in Dateien an.", "ja-JP": "2 人のエディター、1 本のタイムライン。Cairn は両方のバージョンをまとめ、編集を 1 つも失いません。同じ場所を変更していた場合は、ファイル内のダイアログでマージを提案します。", "zh-CN": "两位剪辑师，一条时间线。Cairn 会把两个版本合在一起 - 每个剪辑都保留。当两人改了同一处时，文件视图中会弹出合并对话框。" },
  "merge.expertTitle": { en: "Three-way merge", "de-DE": "Drei-Wege-Zusammenführung", "ja-JP": "3-way マージ", "zh-CN": "三路合并" },
  "merge.semantic": { en: "Semantic", "de-DE": "Semantisch", "ja-JP": "セマンティック", "zh-CN": "语义" },
  "merge.humanOriginal": { en: "Original", "de-DE": "Original", "ja-JP": "オリジナル", "zh-CN": "原版" },
  "merge.baseLabel": { en: "(Base)", "de-DE": "(Base)", "ja-JP": "（ベース）", "zh-CN": "（基线）" },
  "merge.humanYours": { en: "Your changes", "de-DE": "Deine Änderungen", "ja-JP": "あなたの変更", "zh-CN": "你的改动" },
  "merge.oursLabel": { en: "(Ours)", "de-DE": "(Ours)", "ja-JP": "（自分）", "zh-CN": "（己方）" },
  "merge.humanTheirs": { en: "Their changes", "de-DE": "Ihre Änderungen", "ja-JP": "相手の変更", "zh-CN": "对方的改动" },
  "merge.theirsLabel": { en: "(Theirs)", "de-DE": "(Theirs)", "ja-JP": "（相手）", "zh-CN": "（对方）" },
  "merge.run": { en: "Preview merge", "de-DE": "Merge-Vorschau", "ja-JP": "マージをプレビュー", "zh-CN": "预览合并" },
  "merge.noMerge": { en: "No merge yet.", "de-DE": "Noch kein Merge.", "ja-JP": "まだマージしていません。", "zh-CN": "尚未合并。" },
  "merge.searchTitle": { en: "Search clips", "de-DE": "Clips suchen", "ja-JP": "クリップを検索", "zh-CN": "搜索片段" },
  "merge.searchPh": { en: "worried closeup", "de-DE": "besorgte Nahaufnahme", "ja-JP": "不安なクローズアップ", "zh-CN": "担忧的特写" },
  "merge.pickThree": { en: "Pick the three files first: original, your changes, their changes.", "de-DE": "Erst die drei Dateien wählen: Original, deine Änderungen, ihre Änderungen.", "ja-JP": "まず 3 つのファイルを選択：オリジナル、あなたの変更、相手の変更。", "zh-CN": "请先选择三个文件：原版、你的改动、对方的改动。" },
  "merge.running": { en: "Merging…", "de-DE": "Wird zusammengeführt…", "ja-JP": "マージ中…", "zh-CN": "合并中…" },
  "merge.searching": { en: "Searching…", "de-DE": "Suche…", "ja-JP": "検索中…", "zh-CN": "搜索中…" },
  "merge.fallback": { en: "You can also merge from the terminal:", "de-DE": "Du kannst auch im Terminal zusammenführen:", "ja-JP": "ターミナルからもマージできます：", "zh-CN": "也可以从终端合并：" },
  "merge.noMatches": { en: "No matches.", "de-DE": "Keine Treffer.", "ja-JP": "一致なし。", "zh-CN": "没有匹配项。" },
  "merge.outcome": { en: "Merge finished: {o}", "de-DE": "Merge fertig: {o}", "ja-JP": "マージ完了: {o}", "zh-CN": "合并完成：{o}" },
};

function detectLang() {
  const saved = localStorage.getItem("cairn-lang");
  if (saved && STR["nav.home"] && STR["nav.home"][saved]) return saved;
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
  // join(String(x)): Array.prototype.join(undefined) silently joins with
  // "," - a missing var produced "version , is ready" instead of
  // "version ? is ready"
  if (vars) for (const k of Object.keys(vars)) v = v.split(`{${k}}`).join(String(vars[k] ?? "?"));
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
  const cur = $("lang-cur");
  if (cur) cur.textContent = LANG_NAMES[LANG] || LANG;
  document.querySelectorAll(".lang-opt").forEach((b) => {
    const on = b.dataset.lang === LANG;
    b.classList.toggle("is-active", on);
    b.setAttribute("aria-selected", on ? "true" : "false");
  });
  // listbox roving tabindex: exactly one option is tabbable
  if (typeof langRoving === "function") langRoving();
}

const LANG_NAMES = { en: "English", "de-DE": "Deutsch", "ja-JP": "日本語", "zh-CN": "中文" };

/* the topbar language menu (the mockup's globe + "English"): one
   dropdown, closes on outside click and Escape like every other popup.
   Listbox keyboard semantics (a11y round): arrows move, Home/End jump,
   Enter/Space pick, Escape closes back to the button; roving tabindex
   keeps ONE option tabbable per the WAI-ARIA listbox pattern. */
const langOpts = () => [...document.querySelectorAll(".lang-opt")];
function langRoving() {
  const opts = langOpts();
  const i = opts.findIndex((el) => el.dataset.lang === LANG);
  opts.forEach((el, j) => { el.tabIndex = j === i ? 0 : -1; });
}
function langFocus(i) {
  const opts = langOpts();
  const at = (i + opts.length) % opts.length;
  opts.forEach((el, j) => { el.tabIndex = j === at ? 0 : -1; });
  opts[at]?.focus();
}
function setLangOpen(on) {
  const wrap = $("lang-wrap");
  const menu = $("lang-menu");
  if (!wrap || !menu) return;
  wrap.classList.toggle("is-open", on);
  menu.hidden = !on;
  $("lang-btn").setAttribute("aria-expanded", on ? "true" : "false");
  if (on) {
    langRoving();
    const sel = langOpts().findIndex((el) => el.dataset.lang === LANG);
    langFocus(sel < 0 ? 0 : sel);
  }
}

$("lang-btn").addEventListener("click", (ev) => {
  ev.stopPropagation();
  if ($("lang-menu").hidden) setLangOpen(true);
  else { setLangOpen(false); $("lang-btn").focus(); }
});
$("lang-menu").addEventListener("keydown", (ev) => {
  const opts = langOpts();
  const i = opts.indexOf(document.activeElement);
  if (ev.key === "ArrowDown") { ev.preventDefault(); ev.stopPropagation(); langFocus(i < 0 ? 0 : i + 1); }
  else if (ev.key === "ArrowUp") { ev.preventDefault(); ev.stopPropagation(); langFocus(i < 0 ? 0 : i - 1); }
  else if (ev.key === "Home") { ev.preventDefault(); langFocus(0); }
  else if (ev.key === "End") { ev.preventDefault(); langFocus(opts.length - 1); }
  else if (ev.key === "Escape") { ev.stopPropagation(); setLangOpen(false); $("lang-btn").focus(); }
  // Enter/Space on a focused button fires its click natively
});
document.querySelectorAll(".lang-opt").forEach((b) => {
  b.addEventListener("click", () => {
    setLangOpen(false);
    $("lang-btn").focus();
    if (LANG === b.dataset.lang) return;
    LANG = b.dataset.lang;
    localStorage.setItem("cairn-lang", LANG);
    applyI18n();
    rerenderAllDynamic();
  });
});
document.addEventListener("click", (ev) => {
  if (!ev.target.closest("#lang-wrap")) setLangOpen(false);
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
  if (d < 60e3) return t("rel.now");
  if (d < 3600e3) return t("rel.min", { n: Math.floor(d / 60e3) });
  if (d < 86400e3) return t("rel.hour", { n: Math.floor(d / 3600e3) });
  return t("rel.day", { n: Math.floor(d / 86400e3) });
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

/* per-launch dashboard token (the daemon injects it into the page at
   serve time): every /api call carries it — header for fetch, ?t= for
   the two clients that cannot set headers (EventSource, download
   links). Without it the API answers 403. */
function cairnToken() {
  return String(window.CAIRN_TOKEN || "");
}
function authHeaders() {
  return { "X-Cairn-Token": cairnToken() };
}
function withToken(url) {
  return url + (url.includes("?") ? "&" : "?") + "t=" + encodeURIComponent(cairnToken());
}

/* #43 hygiene: every JSON fetch carries an AbortController timeout.
   15s default - long enough for a slow loopback store, short enough
   that a hung request surfaces as an actionable toast instead of a
   spinner forever. Downloads (browser-owned) and the SSE stream are
   deliberately EXCLUDED (they stream; a timeout would kill real work). */
const FETCH_TIMEOUT_MS = 15000;
async function fetchWithTimeout(url, opts, ms) {
  const ctrl = new AbortController();
  const timer = setTimeout(() => ctrl.abort(), ms || FETCH_TIMEOUT_MS);
  try {
    return await fetch(url, { ...opts, signal: ctrl.signal });
  } catch (e) {
    if (e && (e.name === "AbortError" || String(e).includes("abort"))) {
      throw new Error("timeout");
    }
    throw e;
  } finally {
    clearTimeout(timer);
  }
}

async function getJSON(url) {
  const res = await fetchWithTimeout(url, { headers: { Accept: "application/json", ...authHeaders() } });
  if (!res.ok) throw new Error(`${url}: ${res.status}`);
  return res.json();
}

async function postJSON(url, body) {
  const res = await fetchWithTimeout(url, {
    method: "POST",
    headers: { "Content-Type": "application/json", ...authHeaders() },
    body: JSON.stringify(body || {}),
  });
  return res.json();
}

/* #43: busy(btn, label) / done(btn) - a mutating button disables
   itself, says what it is doing (aria-busy for screen readers) and
   restores on completion or failure. Every mutating fetch wraps its
   button in this; a double-click can never double-fire a POST. */
function busy(btn, label) {
  if (!btn || btn.disabled) return null;
  btn.dataset.busyHtml = btn.innerHTML;
  btn.textContent = label;
  btn.disabled = true;
  btn.setAttribute("aria-busy", "true");
  return btn;
}
function done(btn) {
  if (!btn) return;
  if (btn.dataset.busyHtml !== undefined) btn.innerHTML = btn.dataset.busyHtml;
  btn.disabled = false;
  btn.removeAttribute("aria-busy");
}

/* ============================== row ••• menu ==============================
   #44: nothing essential may be hover-only. Every table row carries an
   always-visible ••• button (44px touch target) that opens this menu
   with the row's full action set; the hover quick-actions stay as a
   second affordance. One floating menu element, repositioned per open. */
let MENU_EL = null;
function closeMenus() {
  if (MENU_EL) {
    const anchor = MENU_EL._anchor;
    if (anchor) anchor.setAttribute("aria-expanded", "false");
    MENU_EL.remove();
    MENU_EL = null;
  }
}
function menuOutside(ev) {
  if (MENU_EL && !MENU_EL.contains(ev.target) && ev.target !== MENU_EL._anchor && !MENU_EL._anchor.contains(ev.target)) closeMenus();
}
function openMenu(anchor, items) {
  closeMenus();
  const m = document.createElement("div");
  m.className = "row-menu";
  m.setAttribute("role", "menu");
  m._anchor = anchor;
  for (const it of items || []) {
    if (!it) continue;
    if (it.sep) {
      const s = document.createElement("div");
      s.className = "row-menu-sep";
      m.appendChild(s);
      continue;
    }
    const b = document.createElement("button");
    b.type = "button";
    b.setAttribute("role", "menuitem");
    b.className = "row-menu-item" + (it.danger ? " is-danger" : "");
    b.innerHTML = `${it.icon || ""}<span>${esc(it.label)}</span>`;
    b.addEventListener("click", () => {
      closeMenus();
      if (it.act) it.act(b);
    });
    m.appendChild(b);
  }
  document.body.appendChild(m);
  const r = anchor.getBoundingClientRect();
  const mw = m.offsetWidth, mh = m.offsetHeight;
  let x = Math.max(8, Math.min(r.right - mw, window.innerWidth - mw - 8));
  let y = r.bottom + 6;
  if (y + mh > window.innerHeight - 8) y = Math.max(8, r.top - mh - 6);
  m.style.left = `${x}px`;
  m.style.top = `${y}px`;
  MENU_EL = m;
  anchor.setAttribute("aria-expanded", "true");
  m.addEventListener("keydown", (ev) => {
    const all = [...m.querySelectorAll("button")];
    const i = all.indexOf(document.activeElement);
    if (ev.key === "ArrowDown") { ev.preventDefault(); all[Math.min(i + 1, all.length - 1)]?.focus(); }
    else if (ev.key === "ArrowUp") { ev.preventDefault(); all[Math.max(i - 1, 0)]?.focus(); }
    else if (ev.key === "Escape") { ev.stopPropagation(); closeMenus(); anchor.focus(); }
  });
  window.setTimeout(() => {
    document.addEventListener("click", menuOutside, true);
    const first = m.querySelector("button");
    if (first) first.focus();
  }, 0);
  return m;
}

/* ============================== the confirm modal ==============================
   #38-#41: window.confirm is banned. This in-page dialog follows the
   existing panel patterns: role="dialog" aria-modal, focus TRAP, Escape
   closes, focus returns to the opener. Returns a Promise<boolean>. */
function trapTab(card, ev) {
  const focusables = [...card.querySelectorAll("button, [href], input, select, textarea, [tabindex]:not([tabindex='-1'])")]
    .filter((el) => !el.disabled && el.offsetParent !== null);
  if (!focusables.length) return;
  const first = focusables[0], last = focusables[focusables.length - 1];
  if (ev.shiftKey && document.activeElement === first) { ev.preventDefault(); last.focus(); }
  else if (!ev.shiftKey && document.activeElement === last) { ev.preventDefault(); first.focus(); }
}

function confirmDialog(opts) {
  return new Promise((resolve) => {
    const wrap = $("modal-confirm");
    const card = wrap.querySelector(".modal-card");
    const ok = $("modal-ok"), cancel = $("modal-cancel");
    const prevFocus = document.activeElement;
    $("modal-title").textContent = opts.title || "";
    $("modal-body").textContent = opts.body || "";
    ok.textContent = opts.okLabel || t("modal.cancel");
    ok.className = `btn ${opts.danger === false ? "btn-primary" : "btn-danger"}`;
    function close(v) {
      wrap.hidden = true;
      document.removeEventListener("keydown", onKey, true);
      ok.onclick = cancel.onclick = null;
      if (prevFocus && prevFocus.focus) prevFocus.focus();
      resolve(v);
    }
    function onKey(ev) {
      if (ev.key === "Escape") { ev.stopPropagation(); close(false); }
      else if (ev.key === "Tab") trapTab(card, ev);
    }
    ok.onclick = () => close(true);
    cancel.onclick = () => close(false);
    wrap.hidden = false;
    document.addEventListener("keydown", onKey, true);
    ok.focus();
  });
}

/* toast: inline feedback instead of window.alert (redesign-skill ban);
   aria-live so screen readers announce status changes (a11y round) */
let TOAST_TIMER = null;
function toast(msg, isBad) {
  let el = document.querySelector(".toast");
  if (!el) {
    el = document.createElement("div");
    el.className = "toast";
    el.setAttribute("role", "status");
    el.setAttribute("aria-live", "polite");
    document.body.appendChild(el);
  }
  el.textContent = msg;
  el.setAttribute("aria-live", isBad ? "assertive" : "polite");
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

const VIEWS = ["dashboard", "files", "people", "settings", "howto", "connect", "review", "merge"];
let ACTIVE_VIEW = "dashboard";

function showView(name, focus) {
  if (!VIEWS.includes(name)) name = "dashboard";
  ACTIVE_VIEW = name;
  closeMenus();
  for (const v of VIEWS) {
    const el = $(`view-${v}`);
    if (el) el.classList.toggle("is-active", v === name);
  }
  document.querySelectorAll(".rail-item").forEach((b) => {
    b.classList.toggle("is-active", b.dataset.view === name);
    if (b.dataset.view === name) b.setAttribute("aria-current", "page");
    else b.removeAttribute("aria-current");
  });
  // a view living inside the More disclosure opens its parent so the
  // active state is visible (the disclosure never hides the selection)
  const more = $("nav-more");
  if (more) more.open = ["review", "merge", "connect", "settings", "howto"].includes(name);
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
  "#activity": "dashboard", "#review": "review", "#team": "people", "#people": "people",
  "#live": "dashboard", "#locks": "people", "#versions": "files",
  "#pins": "files", "#recall": "files", "#storage": "settings", "#howto": "howto",
  "#flags": "settings", "#doctor": "settings", "#dashboard": "dashboard",
  "#settings": "settings", "#connect": "connect", "#merge": "merge",
};

/* ============================== shared state ============================== */

let PROJECTS = [];
let HEALTHY = false;
let DAEMON_UP = null;
let VERSION_STR = "";
let LAST_TEAM = null;      // /api/v1/team payload (People view + Home computers count)
let LAST_STATE = null;     // previous derived state (announce transitions, #78)

const FOOT = {
  pending: null, cursor: null, conflicts: null, files: null, synced: null,
  disk: null,
};
let LAST_FILES = null;
let FILES_KEY = null;
let LAST_REVIEW = [];
let LAST_LOCKS = [];
let LAST_ACTIVITY = [];
let LAST_FEED = [];         // feed events (files, pins, leases) with ts
let RECENT_PROJECT = "";    // project id of the newest event (the "recent session")
let LAST_ASSETS = null;     // files payload for the recent-assets panel
let LAST_CHART = null;      // /api/v1/activity payload
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
  // conflicts or a store problem is a human decision away from healthy:
  // "error" (red, needs attention) - the daemon is alive and serving this
  // page. A PROJECT error alone is the sync loop's own retry ladder (5s
  // backoff; it recovers itself) - "retry" (amber), never a red dead chip.
  if ((FOOT.conflicts ?? 0) > 0 || !HEALTHY) return "error";
  if (hasError) return "retry";
  if (inFlight) return "warn";
  return "ok";
}

/* labels + icons per bucket; "bad" stays reserved for the daemon actually
   being down (the page's own fetch failing) */
const CHIP_LABELS = { ok: "chip.ok", warn: "chip.warn", retry: "chip.retry", error: "chip.error", bad: "chip.bad" };
function stateLabel(st) {
  return t(CHIP_LABELS[st] || "chip.bad");
}

/* the chip's icon: a check when everything landed (the mockup's green
   ✓ next to "Synced"), a breathing pip while in flight, an alert when bad */
const CHIP_ICONS = {
  ok: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3.2 8.6l3 3 6.6-7.2"/></svg>',
  warn: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.7-3.75"/><path d="M13 2.6v2.4h-2.4"/></svg>',
  retry: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.7-3.75"/><path d="M13 2.6v2.4h-2.4"/></svg>',
  error: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 1.9l6.3 11H1.7z" stroke-linejoin="round"/><path d="M8 6.2v3.2"/><circle cx="8" cy="11.6" r="0.4" fill="currentColor" stroke="none"/></svg>',
  bad: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 1.9l6.3 11H1.7z" stroke-linejoin="round"/><path d="M8 6.2v3.2"/><circle cx="8" cy="11.6" r="0.4" fill="currentColor" stroke="none"/></svg>',
};

function paintState() {
  const st = deriveState();
  const chip = $("state-chip");
  chip.className = `state-chip is-${st}`;
  $("state-label").textContent = stateLabel(st);
  $("state-ic").innerHTML = CHIP_ICONS[st] || "";

  // the rail carries identity only (version) - the state lives in the
  // topbar chip and the footer dot; three zones showing the same word
  // was the redundancy the review caught
  $("rail-version").textContent = VERSION_STR ? `Cairn ${VERSION_STR}` : "Cairn";

  const sb = document.querySelector(".sb-state");
  if (sb) {
    sb.className = `sb-state is-${st}`;
    $("foot-dot").className = `dot dot-${st === "ok" ? "ok" : st === "warn" || st === "retry" ? "warn" : "bad"}`;
    $("foot-state-label").textContent = stateLabel(st);
  }
  renderHomeStatus(st);
  // #78: transitions into trouble are announced through the live toast
  // channel ("Needs attention"); the happy recovery stays quiet to
  // avoid chatty feedback on every poll cycle
  if (LAST_STATE && LAST_STATE !== st) {
    if ((st === "error" || st === "bad") && LAST_STATE !== "error" && LAST_STATE !== "bad") toast(stateLabel(st), st === "bad");
  }
  LAST_STATE = st;
  renderFooter();
  renderHero();
}

/* ============================== the Home status card ==============================
   Review #13: Home answers ONE question - "is everything okay?".
   The card is the SAME deriveState() that feeds the topbar chip (one
   derivation, two surfaces - they cannot disagree), plus the human
   numbers (files, connected computers) and the issue list, each row
   linking to the view that fixes it. */
function renderHomeStatus(st0) {
  const card = $("home-status");
  if (!card) return;
  const st = st0 || deriveState();
  const issues = [];
  if (DAEMON_UP === false) {
    issues.push({ key: "home.issue.daemon", n: 0, view: "dashboard", cta: "btn.retry", act: () => location.reload() });
  } else {
    const notHere = LAST_FILES && LAST_FILES.ok === true
      ? (LAST_FILES.files || []).filter((f) => f.placeholder).length
      : 0;
    if (notHere > 0) issues.push({ key: notHere === 1 ? "home.issue.files.one" : "home.issue.files", n: notHere, view: "files", cta: "home.issue.filesCta" });
    if ((FOOT.conflicts ?? 0) > 0) issues.push({ key: FOOT.conflicts === 1 ? "home.issue.conflict.one" : "home.issue.conflict", n: FOOT.conflicts, view: "files", cta: "home.issue.conflictCta" });
    const badProjects = PROJECTS.filter((p) => p.state === "error").length;
    if (badProjects > 0) issues.push({ key: badProjects === 1 ? "home.issue.projects.one" : "home.issue.projects", n: badProjects, view: "files", cta: "home.issue.projectsCta" });
  }

  const titleEl = $("hs-title"), subEl = $("hs-sub"), icEl = $("hs-ic");
  card.className = `card home-status is-${st}`;
  if (st === "ok") { titleEl.textContent = t("home.ok.title"); subEl.textContent = t("home.ok.sub"); }
  else if (st === "warn") { titleEl.textContent = t("home.warn.title"); subEl.textContent = t("home.warn.sub"); }
  else if (st === "bad") { titleEl.textContent = t("home.down.title"); subEl.textContent = t("home.down.sub"); }
  else { titleEl.textContent = t("home.attention.title"); subEl.textContent = t("home.attention.sub"); }
  icEl.innerHTML = CHIP_ICONS[st === "error" || st === "retry" ? "error" : st] || "";

  const filesN = FOOT.files ?? 0;
  const hsFiles = $("hs-files");
  if (hsFiles) hsFiles.textContent = String(filesN);

  // computers connected: the team roster (already polled for People);
  // a solo machine says so honestly
  const members = (LAST_TEAM && LAST_TEAM.projects && LAST_TEAM.projects[0] && LAST_TEAM.projects[0].members) || [];
  const nComputers = Math.max(1, members.length);
  const hsComp = $("hs-computers");
  const hsCompLabel = $("hs-computers-label");
  if (hsComp) hsComp.textContent = String(nComputers);
  if (hsCompLabel) hsCompLabel.textContent = nComputers > 1 ? t("home.computers") : t("home.oneComputer");

  const issuesEl = $("hs-issues");
  issuesEl.textContent = "";
  issuesEl.hidden = issues.length === 0;
  for (const iss of issues) {
    const li = document.createElement("li");
    li.className = "hs-issue";
    const text = document.createElement("span");
    text.className = "hs-issue-text";
    text.textContent = t(iss.key, iss.n ? { n: iss.n } : {});
    li.appendChild(text);
    const link = document.createElement("button");
    link.type = "button";
    link.className = "btn btn-ghost btn-sm";
    link.textContent = t(iss.cta);
    link.addEventListener("click", () => { if (iss.act) iss.act(); else showView(iss.view); });
    li.appendChild(link);
    issuesEl.appendChild(li);
  }

  // [Open folder]: reveal the project folder via the daemon's file/open
  // (Windows selects the folder; other file managers open its parent).
  // With no project attached the honest fallback is the Files view.
  const openBtn = $("hs-open");
  if (openBtn) {
    const p = recentProject();
    openBtn.hidden = !p;
    openBtn.onclick = async () => {
      if (!p) { showView("files"); return; }
      const first = LAST_ASSETS && LAST_ASSETS.files && LAST_ASSETS.files[0];
      const path = first ? first.path : "";
      const b = busy(openBtn, t("home.openFolder"));
      try {
        const r = await postJSON("/api/v1/file/open", { project_id: p.project_id, path });
        if (r && r.ok === true) toast(t("toast.openedFolder"));
        else showView("files");
      } catch (e) {
        toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
      } finally {
        done(b || openBtn);
      }
    };
  }
}

/* ============================== hero ============================== */

function renderHero() {
  const sub = $("hero-sub");
  if (!sub) return;
  if (!PROJECTS.length) {
    sub.textContent = t("dash.subEmpty");
    return;
  }
  sub.textContent = t("dash.sub", { n: PROJECTS.length, files: FOOT.files ?? 0, synced: FOOT.synced ?? 0 });
}

/* round 27: the path field left the hero (it is a power-user affordance
   squatting on prime real estate - the retro's words). "+ Add to
   Workspace" opens the Add panel, where Browse (the OS folder picker)
   leads and the text field follows. A bare token typed into the PANEL
   still matches a known project id or name and opens it; a path
   attaches; anything else goes to the daemon, which answers honestly. */
function looksLikePath(v) {
  return /[\\/]/.test(v) || /^[A-Za-z]:/.test(v);
}

async function doWorkspaceAdd() {
  openPanel("panel-add");
  // focus Browse's sibling input only when the picker is unavailable
  // (non-Windows hosts) so typing is one tab away, never a dead end
  try {
    const r = await fetch("/api/v1/pick-folder", { method: "GET", headers: { ...authHeaders() } });
    if (r.ok) {
      const j = await r.json();
      if (j && j.ok === true && !j.cancelled && j.path) {
        $("attach-root").value = j.path;
        $("attach-project").focus();
        return;
      }
    }
  } catch { /* daemon probe failed - the panel's own fields carry */ }
  $("attach-root").focus();
}

/* the OS folder picker (round 27 "click, don't type"): the daemon
   serves a native dialog from the user's session. Non-Windows or
   headless hosts return cancelled/unavailable - the caller falls back
   to the text field with a quiet toast, never an error wall. */
async function pickFolder(targetInput) {
  let j = null;
  try {
    // auth header + no timeout on purpose: the native dialog waits for
    // a human, so the 15s fetch budget must not kill it (same exclusion
    // class as the SSE stream and downloads)
    const r = await fetch("/api/v1/pick-folder", { method: "GET", headers: { ...authHeaders() } });
    j = await r.json();
  } catch { j = null; }
  if (j && j.ok === true) {
    if (j.cancelled) { toast(t("toast.pickCancelled")); return; }
    if (j.unsupported) { toast(t("toast.pickUnavailable")); targetInput.focus(); return; }
    if (j.path) {
      targetInput.value = j.path;
      targetInput.dispatchEvent(new Event("change"));
      toast(j.path);
      return;
    }
  }
  toast(t("toast.pickUnavailable"));
  targetInput.focus();
}

$("btn-add-ws").addEventListener("click", doWorkspaceAdd);
$("btn-browse").addEventListener("click", (ev) => {
  ev.preventDefault();
  pickFolder($("attach-root"));
});
$("ob-browse").addEventListener("click", async (ev) => {
  ev.preventDefault();
  const browseBtn = ev.currentTarget; // captured before any await
  const input = $("ob-root");
  await pickFolder(input);
  // single-button polish: if picker filled a path, auto-attach immediately (no second click)
  const path = input && input.value.trim();
  if (path) {
    const btn = $("ob-attach-btn");
    if (btn) btn.click();
    else {
      // fallback direct attach when attach button hidden
      const b = busy(browseBtn, t("busy.attach"));
      try {
        const r = await postJSON("/api/v1/attach", { path });
        if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
        else { toast(t("toast.attached")); refreshAll(); }
      } catch (e) { toast(t("toast.failed", { e: String(e) }), true); }
      finally { done(b || browseBtn); }
    }
  } else {
    // picker unavailable — reveal manual attach row
    const scene = $("ob-attach");
    if (scene) scene.hidden = false;
  }
});

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
  const sub2 = $("ob-sub2");
  if (sub2) sub2.textContent = t("ob.sub2");
  const hint = document.querySelector(".ob-hint");
  if (hint) hint.textContent = t("ob.hint");
  const oc = $("ob-continue");
  if (oc) oc.textContent = t("ob.continue");

  // single-button onboard: stage is 1/1, not 1/3
  $("foot-track-fill").style.width = `100%`;
  $("foot-progress").setAttribute("aria-valuenow", "1");
  $("foot-stage-label").textContent = t("btn.browse");
}

/* single-button onboard: Browse is the only CTA — no Continue two-step */
const _obContinue = $("ob-continue");
if (_obContinue) _obContinue.addEventListener("click", () => {
  const scene = $("ob-attach");
  if (scene && scene.hidden) {
    scene.hidden = false;
    _obContinue.hidden = true;
    $("ob-root") && $("ob-root").focus();
  }
});

/* #25: "Try again" on the zero-root error card just re-runs the status
   probe (the honest version of reload-without-reload); if the daemon
   is back the whole app unhides through the normal paint path */
$("ob-retry")?.addEventListener("click", async (ev) => {
  const b = busy(ev.currentTarget, t("busy.working"));
  try { await refreshStatus(); await refreshProjects(); } finally { done(b || ev.currentTarget); }
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
    const volLabel = $("storage-volume-label");

    // round 27: the meter reads the WORKSPACE volume when one is
    // attached (that is where the editor's media lives), else the store
    // volume - and it SAYS which. Neutral zinc fill: amber reads as a
    // warning (and matched the reconnecting pill); color only appears
    // past 90%, red past 95% - usage is only a problem at the edge.
    const vols = Array.isArray(r.volumes) ? r.volumes : [];
    const workspace = vols.find((v) => v.label !== "store") || vols[0] || r.disk;
    const store = r.disk;
    if (store && Number.isFinite(store.free_bytes) && Number.isFinite(store.total_bytes)) {
      FOOT.disk = { total: store.total_bytes, free: store.free_bytes };
      renderFooter();
    }
    if (workspace && Number.isFinite(workspace.free_bytes) && Number.isFinite(workspace.total_bytes) && workspace.total_bytes > 0) {
      const used = Math.max(0, workspace.total_bytes - workspace.free_bytes);
      const pct = Math.min(100, (used / workspace.total_bytes) * 100);
      const fill = $("quota-fill");
      fill.style.width = `${Math.max(2, Math.round(pct))}%`;
      fill.style.background = pct >= 95 ? "#d96b66" : pct >= 90 ? "#d9a13c" : "var(--fill-neutral, #9aa0a6)";
      $("set-quota").textContent = `${fmtBytes(used)} / ${fmtBytes(workspace.total_bytes)}`;
      if (volLabel) {
        const isStore = !vols.length || workspace.label === "store" || workspace === r.disk;
        volLabel.textContent = isStore ? t("vol.store") : t("vol.project", { p: workspace.label });
        volLabel.title = `${Math.round(pct)}% used · live from the OS`;
      }
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

/* ============================== recent sessions panel ==============================
   Four sections, one card: the newest session (click jumps to its
   files), the other active projects as quiet chips, the latest actions
   as an icon timeline, and the activity chart. All store-derived. */

function recentProject() {
  return (
    PROJECTS.find((p) => p.project_id === RECENT_PROJECT) || PROJECTS[0] || null
  );
}

function jumpToProject(pid) {
  const sel = $("files-project");
  if (sel && PROJECTS.some((x) => x.project_id === pid)) sel.value = pid;
  showView("files", "files-filter");
  refreshFiles();
}

/* ============================== render-on-change ==============================
   Round 27, the "lettering keeps flashing" retro: the 2s poll re-rendered
   the sessions/actions/projects/assets panels from scratch every cycle,
   and every fresh DOM row replayed its rise/stagger animation - the
   console looked like it was strobing while sync retried. Fix at the
   root: a panel only re-renders when its CONTENT signature changed.
   Same data in = same DOM out; the animation plays once, on real
   change. (relTime strings are part of the signature, so "2 min ago"
   -> "3 min ago" still updates.) */
const RENDER_SIGS = new Map();
function renderIfChanged(key, sig, render) {
  // LANG prefixes every signature: a language switch must repaint even
  // when the underlying data is unchanged (row labels are translated)
  const full = LANG + "::" + sig;
  if (RENDER_SIGS.get(key) === full) return;
  RENDER_SIGS.set(key, full);
  render();
}
function sigOf(list, pick) {
  if (!list || !list.length) return "0";
  return list.map(pick).join("|");
}

function renderRecentSession() {
  const p0 = recentProject();
  renderIfChanged(
    "rs-session",
    p0 ? `${p0.project_id}:${p0.files_synced ?? 0}:${p0.pending_outbox ?? 0}:${p0.state ?? ""}` : "none",
    () => renderRecentSessionInner(),
  );
}

function renderRecentSessionInner() {
  const box = $("rs-session-body");
  box.textContent = "";
  const p = recentProject();
  if (!p) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.projects");
    box.appendChild(empty);
    return;
  }
  const total = Number(p.files_synced ?? 0) + Number(p.pending_outbox ?? 0) || Number(p.files_synced ?? 0);
  const row = document.createElement("button");
  row.type = "button";
  row.className = "rs-session";
  row.innerHTML =
    `<span class="rs-name" title="${esc(p.root_path ?? "")}">${esc(p.display_name || p.project_id)}</span>` +
    `<span class="rs-sub">${esc(t("rs.summary", { synced: p.files_synced ?? 0, files: total }))}</span>`;
  row.addEventListener("click", () => jumpToProject(p.project_id));
  box.appendChild(row);
}

function renderActiveProjects() {
  const current = recentProject();
  const others = PROJECTS.filter((p) => !current || p.project_id !== current.project_id).slice(0, 4);
  renderIfChanged(
    "rs-projects",
    sigOf(others, (p) => `${p.project_id}:${p.state ?? ""}`),
    () => renderActiveProjectsInner(),
  );
}

function renderActiveProjectsInner() {
  const sec = $("rs-projects");
  const box = $("rs-projects-body");
  box.textContent = "";
  const current = recentProject();
  const others = PROJECTS.filter((p) => !current || p.project_id !== current.project_id).slice(0, 4);
  // no other projects -> the whole section folds away (no stub row)
  sec.hidden = others.length === 0;
  for (const p of others) {
    const chip = document.createElement("button");
    chip.type = "button";
    chip.className = "proj-chip";
    const st = p.state === "error" ? "bad" : p.state === "syncing" ? "warn" : "ok";
    chip.innerHTML =
      `<span class="dot dot-${st === "ok" ? "ok" : st === "warn" || st === "retry" ? "warn" : "bad"}"></span>` +
      `<span>${esc(p.display_name || p.project_id)}</span>`;
    chip.title = esc(p.root_path ?? "");
    chip.addEventListener("click", () => jumpToProject(p.project_id));
    box.appendChild(chip);
  }
  stagger(box, ".proj-chip");
}

/* ============================== latest actions ==============================
   The feed's real events, humanized: a pin icon for pins, a refresh
   icon for synced/saved files, a folder icon for a held lock ("the
   project is open in the NLE right now"). Icons live in quiet circles. */

const ACT_ICONS = {
  pin: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M9.6 2.3l4.1 4.1-1.1 1.1-1-.3-2.9 2.9.3 2.2-1 1-3-3-3.4 3.4 3.4-3.4-3-3 1-1 2.2.3 2.9-2.9-.3-1z"/></svg>',
  sync: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" aria-hidden="true"><path d="M13 8a5 5 0 1 1-1.7-3.75"/><path d="M13 2.6v2.4h-2.4"/></svg>',
  folder: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true"><path d="M1.8 4.2c0-.6.5-1.1 1.1-1.1h3l1.5 1.7h5.7c.6 0 1.1.5 1.1 1.1v6c0 .6-.5 1.1-1.1 1.1H2.9c-.6 0-1.1-.5-1.1-1.1z"/></svg>',
  rename: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.5 8h11M10 4.5L13.5 8 10 11.5"/></svg>',
  trash: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M2.8 4.5h10.4M6.2 4.5V3.2c0-.4.3-.7.7-.7h2.2c.4 0 .7.3.7.7v1.3M4 4.5l.6 8.4c0 .4.3.7.7.7h5.4c.4 0 .7-.3.7-.7l.6-8.4"/></svg>',
  alert: '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 1.9l6.3 11H1.7z"/><path d="M8 6.2v3.2"/><circle cx="8" cy="11.6" r="0.4" fill="currentColor" stroke="none"/></svg>',
};

function actMeta(e) {
  const kind = e.kind || "upsert";
  if (kind === "pinned") return [ACT_ICONS.pin, "ok", "act.pinned"];
  if (kind === "unpinned") return [ACT_ICONS.pin, "warn", "act.unpinned"];
  if (kind === "lease") return [ACT_ICONS.folder, "info", "act.opened"];
  if (kind === "rename") return [ACT_ICONS.rename, "info", "act.renamed"];
  if (kind === "delete") return [ACT_ICONS.trash, "bad", "act.deleted"];
  if (kind === "conflict") return [ACT_ICONS.alert, "bad", "files.conflict"];
  const synced = (e.state || "") === "synced";
  return [ACT_ICONS.sync, synced ? "ok" : "warn", synced ? "act.synced" : "act.saved"];
}

function renderActions() {
  const rows0 = LAST_ACTIVITY.slice(0, 6);
  renderIfChanged(
    "rs-actions",
    sigOf(rows0, (e) => `${e.path ?? ""}:${e.kind ?? ""}:${e.state ?? ""}:${e.ts ?? 0}`),
    () => renderActionsInner(),
  );
}

function renderActionsInner() {
  const box = $("rs-actions-body");
  box.textContent = "";
  const rows = LAST_ACTIVITY.slice(0, 6);
  if (!rows.length) {
    const empty = document.createElement("p");
    empty.className = "empty-note";
    empty.textContent = t("empty.activity");
    box.appendChild(empty);
    return;
  }
  rows.forEach((e, i) => {
    const [icon, cls, verbKey] = actMeta(e);
    const row = document.createElement("div");
    row.className = "act-row";
    row.style.setProperty("--i", String(i));
    const [, base] = splitPath(e.path || "");
    row.innerHTML =
      `<span class="act-ic ${cls}">${icon}</span>` +
      `<span class="act-text" title="${esc(e.path ?? "")}">` +
      `<span class="act-path">${esc(base || e.path || "")}</span> ` +
      `<span class="act-verb">${esc(t(verbKey))}</span>` +
      `</span>` +
      `<span class="act-when">${esc(relTime(e.ts))}</span>`;
    box.appendChild(row);
  });
}

function renderFeed(entries) {
  LAST_ACTIVITY = entries || [];
  LAST_FEED = LAST_ACTIVITY;
  const first = LAST_ACTIVITY.find((e) => e.project);
  RECENT_PROJECT = first ? String(first.project) : "";
  renderRecentSession();
  renderActiveProjects();
  renderActions();
}

function renderProjectsSettings() {
  renderIfChanged(
    "proj-list",
    sigOf(PROJECTS, (p) => `${p.project_id}:${p.state ?? ""}:${p.display_name ?? ""}`),
    () => renderProjectsSettingsInner(),
  );
}

function renderProjectsSettingsInner() {
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
      `<span class="row-actions">` +
      `<button type="button" class="btn btn-ghost btn-sm" data-detach="${esc(p.project_id)}">${esc(t("btn.detach"))}</button>` +
      `<button type="button" class="btn btn-dots" data-dots="${esc(p.project_id)}" aria-label="${esc(t("a11y.rowMenu"))} · ${esc(p.display_name || p.project_id)}" aria-haspopup="menu" aria-expanded="false" title="${esc(t("a11y.rowMenu"))}">${DOTS_IC}</button>` +
      `</span>` +
      `</div>` +
      `<div class="proj-root">` +
      `<code title="${esc(p.root_path ?? "")}">${esc(root)}</code>` +
      `<button type="button" class="btn btn-icon btn-copy" data-copy="${esc(p.root_path ?? "")}" aria-label="${esc(t("a11y.copyPath"))}">` +
      `<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5v-2a1.5 1.5 0 0 0-1.5-1.5H4A1.5 1.5 0 0 0 2.5 3.5v5A1.5 1.5 0 0 0 4 10h1.5"/></svg>` +
      `</button></div>` +
      (p.last_error ? `<p class="proj-err">${esc(p.last_error)}</p>` : "");
    div.querySelector("[data-detach]").addEventListener("click", (ev) => doDetachFlow(String(ev.currentTarget.dataset.detach), ev.currentTarget));
    // the ••• equivalent of the right-click menu (#44): Remove from Cairn
    div.querySelector("[data-dots]").addEventListener("click", (ev) => {
      ev.stopPropagation();
      const dots = ev.currentTarget;
      openMenu(dots, [
        { icon: OPEN_IC, label: t("btn.open"), act: (b) => doFileOpen(p.project_id, "", b) },
        { icon: DUP_IC, label: t("a11y.copyPath"), act: () => copyText(p.root_path || "").then((ok) => toast(ok ? t("toast.copied") : t("toast.denied", { e: t("err.clipboard") }), !ok)) },
        { sep: true },
        { icon: DOTS_IC, label: t("btn.detach"), danger: true, act: () => doDetachFlow(p.project_id, dots) },
      ]);
    });
    // right-click delete (user asked: "i cant right click and delete")
    div.addEventListener("contextmenu", (ev) => {
      ev.preventDefault();
      doDetachFlow(p.project_id, null);
    });
    const copyBtnEl = div.querySelector(".btn-copy");
    if (copyBtnEl) copyBtnEl.addEventListener("click", copyBtn);
    list.appendChild(div);
  }
}

/* detach with the human confirm modal (#38-#41): consequences first -
   the files STAY on this computer, only the syncing stops */
async function doDetachFlow(pid, btn) {
  if (!pid) return;
  const ok = await confirmDialog({
    title: t("confirm.detachTitle"),
    body: t("confirm.detachBody"),
    okLabel: t("btn.removeFolder"),
  });
  if (!ok) return;
  const b = busy(btn, t("busy.working"));
  try {
    const r = await postJSON("/api/v1/detach", { project_id: pid });
    if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
    else toast(t("toast.detached"));
    refreshAll();
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally {
    done(b || btn);
  }
}

async function refreshProjects() {
  try {
    const r = await getJSON("/api/v1/projects");
    PROJECTS = r.projects || [];
    fillProjectSelects();
    renderRecentSession();
    renderActiveProjects();
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
  // placeholder rows arrive with a normal state ("synced" - the manifest
  // IS synced) plus placeholder:true; the human truth is "Online only",
  // so the flag wins over the state word (views.rs file_badge never says
  // "placeholder" - only the boolean does)
  if (f.placeholder) return ["dim", "files.onlineOnly"];
  const st = f.state || "syncing";
  if (st === "synced") return ["ok", "files.synced"];
  if (st === "conflict") return ["bad", "files.conflict"];
  if (st === "syncing") return ["warn", "files.syncing"];
  return ["dim", "files.onlineOnly"];
}

/* proxy (editing copies): the contract gives media rows a proxy_state
   ("none|ready|stale|failed"). "ready" earns a subtle badge; "none" and
   "stale" earn a Make-editing-copy action in the row menu. While a
   generate call runs (or the daemon answers 409 in_progress) the row
   shows the human "Making editing copy…" state. */
const PROXY_PENDING = new Set();
function proxyBadge(f) {
  if (PROXY_PENDING.has(f.path)) return `<span class="f-proxy is-working">${esc(t("proxy.making"))}</span>`;
  if (f.proxy_state === "ready") return `<span class="f-proxy is-ready">${esc(t("proxy.ready"))}</span>`;
  return "";
}
function isMediaPath(p) {
  const ext = String(p || "").split(".").pop().toLowerCase();
  return FILM_EXT.includes(ext) || AUDIO_EXT.includes(ext);
}
async function doProxyGenerate(project, path, btn) {
  if (!project || !path) return;
  if (PROXY_PENDING.has(path)) { toast(t("proxy.exists")); return; }
  PROXY_PENDING.add(path);
  const b = busy(btn, t("busy.working"));
  try {
    // contract freeze: POST /api/v1/proxy/generate {"project":..,"path":..}
    //   -> 200 {ok:true, proxy_rel, bytes, state:"ready"}
    //   | 409 {ok:false, error:"in_progress"} | 400/404 {ok:false, error}
    const r = await postJSON("/api/v1/proxy/generate", { project, path });
    if (r && r.ok === true) {
      // the returned proxy_rel is surfaced (contract: the copy lands at
      // .cairn/proxy-cache/...) - people can see where the lighter file
      // will appear; older daemons without the field get the plain line
      toast(r.proxy_rel ? t("proxy.startedRel", { p: r.proxy_rel }) : t("proxy.started"));
      PROXY_PENDING.delete(path);
      refreshFiles();
    } else if (r && r.error === "in_progress") {
      toast(t("proxy.exists"));
    } else {
      PROXY_PENDING.delete(path);
      toast(t("proxy.fail"), true);
      refreshFiles();
    }
  } catch (e) {
    PROXY_PENDING.delete(path);
    // the route may not exist on older daemons (2-d lands it in
    // parallel) - the honest surface is the same human toast
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("proxy.fail"), true);
    refreshFiles();
  } finally {
    done(b || btn);
    renderFiles(LAST_FILES);
  }
}

/* the ••• row menu content: every action the row supports, human-labeled,
   nothing hover-only (#44) */
function fileRowMenuItems(f, project) {
  const items = [];
  if (f.merge_available) {
    items.push({ icon: MERGE_IC, label: t("files.mergeAvailable"), act: () => mergeDialog(project, f.path) });
  }
  if (f.placeholder) {
    items.push({ icon: DL_IC, label: t("btn.makeAvailable"), act: (b) => doRecallWithBusy(project, f.path, b) });
  }
  items.push({
    icon: PIN_IC,
    label: f.pinned ? t("btn.stopKeeping") : t("btn.keepHere"),
    act: (b) => doFilePin(project, f.path, f.pinned, b),
  });
  items.push({ sep: true });
  items.push({ icon: OPEN_IC, label: t("btn.open"), act: (b) => doFileOpen(project, f.path, b) });
  items.push({ icon: DL_IC, label: t("btn.download"), act: (b) => doFileDownload(project, f.path, b) });
  if (!f.placeholder) {
    items.push({ icon: DUP_IC, label: t("btn.duplicate"), act: (b) => doFileDuplicate(project, f.path, b) });
  }
  if (!f.placeholder && f.proxy_state && ["none", "stale", "failed"].includes(f.proxy_state) && isMediaPath(f.path)) {
    items.push({ icon: FILM_IC, label: t("proxy.make"), act: (b) => doProxyGenerate(project, f.path, b) });
  }
  items.push({ sep: true });
  items.push({ icon: SHARE_IC, label: t("btn.shareLink"), act: (b) => doFileShare(project, f.path, b) });
  return items;
}

function dotsBtn(label) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "btn btn-dots";
  btn.setAttribute("aria-label", label);
  btn.setAttribute("aria-haspopup", "menu");
  btn.setAttribute("aria-expanded", "false");
  btn.title = label;
  btn.innerHTML = DOTS_IC;
  return btn;
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
  const project = selectedProject();
  // render-on-change (round 27): the same 300 rows re-creating every
  // 2s was half the strobing; the signature covers path/size/state/
  // pinned/placeholder - anything the row can visually say
  const files = (r && r.files) || [];
  renderIfChanged(
    "files-body",
    `${project}::${sigOf(files, (f) => `${f.path}:${f.size ?? 0}:${f.state ?? ""}:${f.pinned ? 1 : 0}:${f.placeholder ? 1 : 0}:${f.merge_available ? 1 : 0}:${f.proxy_state ?? ""}:${PROXY_PENDING.has(f.path) ? 1 : 0}`)}`,
    () => renderFilesInner(r, project),
  );
  if (sum) {
    const s = (r && r.summary) || {};
    if (!files.length) sum.textContent = "";
    else sum.textContent = t("files.summary", {
      files: s.files ?? 0, synced: s.synced ?? 0, syncing: s.syncing ?? 0, conflict: s.conflict ?? 0,
    });
  }
}

function renderFilesInner(r, project) {
  const body = $("files-body");
  if (!body) return;
  body.innerHTML = "";
  if (!r || r.ok !== true || !r.files || r.files.length === 0) {
    body.innerHTML = `<tr><td colspan="4" class="empty">${esc(t("empty.files"))}</td></tr>`;
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

  for (const f of r.files.slice(0, 300)) {
    const [dir, base] = splitPath(f.path);
    const [cls, key] = stFor(f);
    const tr = document.createElement("tr");

    const tdName = document.createElement("td");
    tdName.innerHTML =
      `<span class="f-name">${fileIcon(f.path)}<span class="f-base" title="${esc(f.path)}">${esc(base)}</span></span>` +
      (dir ? `<span class="f-dir">${esc(dir)}</span>` : "");

    const tdSize = document.createElement("td");
    tdSize.className = "num";
    tdSize.textContent = fmtBytes(f.size);

    const tdState = document.createElement("td");
    tdState.innerHTML =
      `<span class="f-state s-${cls}">${ST_ICONS[cls]}<span>${esc(t(key))}</span>` +
      (f.pinned ? `<span class="f-pinmark" title="${esc(t("files.pinnedA11y"))}">${PIN_IC}</span>` : "") +
      `</span>` +
      proxyBadge(f) +
      // CONTRACT-DEBT #1: the merge offer shows as a real, always
      // reachable chip - clicking it opens the human merge dialog
      (f.merge_available ? ` <button type="button" class="chip-btn is-merge" title="${esc(t("files.mergeAvailable"))}">${MERGE_IC}<span>${esc(t("files.mergeAvailable"))}</span></button>` : "");

    const tdAct = document.createElement("td");
    const rowActions = document.createElement("div");
    rowActions.className = "row-actions";
    // round 27: the four quick-actions the retro asked for by name -
    // Open, Download, Duplicate, Share - plus the keep/copy/make-
    // available affordances. These stay as the SECOND affordance; the
    // ••• menu below is the primary, always-visible one (#44).
    const openBtn = qaBtn("open", t("btn.open"), f.path);
    const dlBtn = qaBtn("download", t("btn.download"), f.path);
    const dupBtn = qaBtn("duplicate", t("btn.duplicate"), f.path);
    const shareBtn = qaBtn("share", t("btn.shareLink"), f.path);
    const pinBtn = fileActionBtn("pin", f.pinned ? t("btn.unpin") : t("btn.pin"), f.path);
    pinBtn.dataset.act = f.pinned ? "unpin" : "pin";
    pinBtn.style.color = f.pinned ? "var(--warn-fg)" : "";
    rowActions.appendChild(openBtn);
    rowActions.appendChild(dlBtn);
    rowActions.appendChild(dupBtn);
    rowActions.appendChild(shareBtn);
    rowActions.appendChild(pinBtn);
    rowActions.appendChild(fileActionBtn("copy", t("btn.copy"), f.path));
    if (f.placeholder) rowActions.appendChild(fileActionBtn("recall", t("btn.recall"), f.path));
    const dots = dotsBtn(`${t("a11y.rowMenu")} · ${base}`);
    rowActions.appendChild(dots);
    tdAct.appendChild(rowActions);

    openBtn.addEventListener("click", () => doFileOpen(project, f.path));
    dlBtn.addEventListener("click", () => doFileDownload(project, f.path));
    dupBtn.addEventListener("click", () => doFileDuplicate(project, f.path));
    shareBtn.addEventListener("click", () => doFileShare(project, f.path));
    pinBtn.addEventListener("click", () => doFilePin(project, f.path, f.pinned));
    dots.addEventListener("click", (ev) => {
      ev.stopPropagation();
      openMenu(dots, fileRowMenuItems(f, project));
    });
    tdAct.querySelector('[data-act="copy"]').addEventListener("click", copyBtn);
    const recallBtnEl = tdAct.querySelector('[data-act="recall"]');
    if (recallBtnEl) recallBtnEl.addEventListener("click", () => doRecall(project, f.path));
    const mergeChip = tdState.querySelector(".chip-btn.is-merge");
    if (mergeChip) mergeChip.addEventListener("click", () => mergeDialog(project, f.path));

    tr.append(tdName, tdSize, tdState, tdAct);
    body.appendChild(tr);
  }
}

/* a quick-action button (Open/Download/Duplicate/Share): icon + label,
   dataset carries the action for delegation */
function qaBtn(act, label, path) {
  const btn = document.createElement("button");
  btn.type = "button";
  btn.className = "btn btn-icon btn-qa btn-qa-labeled";
  btn.setAttribute("aria-label", label);
  btn.title = label;
  btn.dataset.path = path;
  btn.dataset.act = act;
  btn.innerHTML = act === "open" ? OPEN_IC : act === "download" ? DL_IC : act === "duplicate" ? DUP_IC : act === "merge-accept" ? MERGE_IC : act === "merge-decline" ? DEC_IC : SHARE_IC;
  return btn;
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
    // the skeleton is NOT a signature state: invalidate so the real
    // rows always land, even when the recovered data matches the old
    // signature (otherwise the skeleton could freeze on screen)
    RENDER_SIGS.delete("files-body");
    renderFilesSkeleton();
  }
  const q = filter ? `&q=${encodeURIComponent(filter)}` : "";
  try {
    LAST_FILES = await getJSON(`/api/v1/files?project=${encodeURIComponent(project)}${q}`);
    renderFiles(LAST_FILES);
  } catch {
    if (!LAST_FILES || LAST_FILES.ok !== true) {
      // same invalidation for the error rows: recovery with identical
      // data must still re-render
      RENDER_SIGS.delete("files-body");
      renderFilesError();
    }
  }
}

async function doFilePin(project, path, pinned, btn) {
  if (!project || !path) return;
  const url = pinned ? "/api/v1/pins/unpin" : "/api/v1/pins";
  const b = busy(btn, t("busy.working"));
  try {
    const r = await postJSON(url, { project_id: project, path });
    if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
    else toast(pinned ? t("toast.unpinned") : t("toast.pinned"));
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally {
    done(b || btn);
  }
  refreshFiles();
  refreshAssetsPanel();
  refreshFeed();
}

/* ============================== file quick-actions ==============================
   Round 27: rows are no longer static. Open reveals in the OS file
   manager, Download streams the bytes, Duplicate copies beside the
   original, Share puts the path on the clipboard. All against the
   daemon's loopback endpoints; all honest about placeholders (a
   0-byte placeholder cannot be downloaded or duplicated - the daemon
   says recall-first, we show it). */

const OPEN_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linejoin="round" aria-hidden="true"><path d="M2.2 7.4c0-2.3 1.9-4.2 4.2-4.2h6.2c.8 0 1.4.6 1.4 1.4v6.2c0 2.3-1.9 4.2-4.2 4.2H3.6c-.8 0-1.4-.6-1.4-1.4z"/><path d="M5.4 8.3l1.9-1.9c.4-.4 1-.4 1.4 0l1.9 1.9"/><path d="M8 6.7v4.2"/></svg>';
const DL_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M8 2.5v7.5M4.5 7L8 10.5 11.5 7M3 13h10"/></svg>';
const DUP_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linejoin="round" aria-hidden="true"><rect x="5.5" y="5.5" width="8" height="8" rx="1.5"/><path d="M10.5 5.5V4A1.5 1.5 0 0 0 9 2.5H4A1.5 1.5 0 0 0 2.5 4v5A1.5 1.5 0 0 0 4 10.5h1.5"/></svg>';
const SHARE_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" aria-hidden="true"><circle cx="12.5" cy="3.5" r="1.7"/><circle cx="3.5" cy="8" r="1.7"/><circle cx="12.5" cy="12.5" r="1.7"/><path d="M10.9 4.5l-6.8 2.6M5.1 8.9l6.8 2.6"/></svg>';
/* CONTRACT-DEBT #1: the merge-offer badge (three strands joining) and its
   decline twin (same mark, crossed out). */
const MERGE_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 3v3.5c0 2.5 2 4.5 5 4.5s5-2 5-4.5V3"/><path d="M8 11v2.5"/><circle cx="8" cy="9.5" r="1.2"/></svg>';
const DEC_IC = '<svg class="ic" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M3 3v3.5c0 2.5 2 4.5 5 4.5s5-2 5-4.5V3"/><path d="M8 11v2.5"/><circle cx="8" cy="9.5" r="1.2"/><path d="M2.5 13.5l11-11"/></svg>';
/* the ••• affordance (three dots) - shared by every row menu button */
const DOTS_IC = '<svg viewBox="0 0 16 16" aria-hidden="true"><circle cx="3" cy="8" r="1.5" fill="currentColor"/><circle cx="8" cy="8" r="1.5" fill="currentColor"/><circle cx="13" cy="8" r="1.5" fill="currentColor"/></svg>';

async function doFileOpen(project, path, btn) {
  if (!project || !path) return;
  const b = busy(btn, t("busy.working"));
  try {
    const r = await postJSON("/api/v1/file/open", { project_id: project, path });
    if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
    else toast(t("toast.openedFolder"));
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally {
    done(b || btn);
  }
}

async function doFileDownload(project, path) {
  if (!project || !path) return;
  const url = withToken(`/api/v1/file/download?project=${encodeURIComponent(project)}&path=${encodeURIComponent(path)}`);
  // honest UX: a placeholder answers 409 AFTER the browser already said
  // "downloading..." - preflight one cheap HEAD and route the failure to
  // the action that actually fixes it (recall), not to a dead download.
  try {
    const head = await fetch(url, { method: "HEAD" });
    if (head.status === 409) { toast(t("toast.recallFirst"), true); return; }
    if (!head.ok) { toast(t("toast.denied", { e: "HTTP " + head.status }), true); return; }
  } catch { /* daemon hiccup: let the browser attempt it anyway */ }
  // the browser owns the download UX (progress, cancel, save location);
  // we just point it at the daemon's streaming endpoint
  const a = document.createElement("a");
  a.href = url;
  a.download = "";
  document.body.appendChild(a);
  a.click();
  a.remove();
  toast(t("toast.downloading"));
}

async function doFileDuplicate(project, path, btn) {
  if (!project || !path) return;
  const b = busy(btn, t("busy.duplicate"));
  try {
    const r = await postJSON("/api/v1/file/duplicate", { project_id: project, path });
    if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
    else {
      toast(t("toast.duplicated"));
      refreshFiles();
      refreshAssetsPanel();
    }
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally {
    done(b || btn);
  }
}

/* CONTRACT-DEBT #1: the conflict auto-offer. Accept re-merges the three
   sources on the daemon and syncs the result as ONE journal entry; Decline
   only removes the affordance - the conflict copy stays (that is the sync
   contract: declining never discards an edit). Both go busy() (#43). */
async function doMergeAccept(project, path, btn) {
  if (!project || !path) return;
  const b = busy(btn, t("busy.merge"));
  try {
    const r = await postJSON("/api/v1/merge/offer/accept", { project_id: project, path });
    if (r && r.ok === false) toast(t("toast.mergeFail"), true);
    else {
      toast(t("toast.mergeAccepted"));
      refreshFiles();
      refreshFeed();
    }
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.mergeFail"), true);
  } finally {
    done(b || btn);
  }
}

async function doMergeDecline(project, path, btn) {
  if (!project || !path) return;
  const b = busy(btn, t("busy.working"));
  try {
    const r = await postJSON("/api/v1/merge/offer/decline", { project_id: project, path });
    if (r && r.ok === false) toast(t("toast.denied", { e: r.error }), true);
    else {
      toast(t("toast.mergeDeclined"));
      refreshFiles();
    }
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally {
    done(b || btn);
  }
}

/* the human merge-offer dialog (CONTRACT-DEBT #1, review #89): the row
   chip / menu opens it; Merge = accept, Keep both = decline - the exact
   bodies dashboard.rs expects ({project_id, path}). No preview route
   exists on the backend, so no Preview button is offered. */
function mergeDialog(project, path) {
  const wrap = $("modal-merge");
  if (!wrap) return;
  const card = wrap.querySelector(".modal-card");
  const prevFocus = document.activeElement;
  $("merge-dlg-path").textContent = path;
  const okBtn = $("merge-dlg-merge"), keepBtn = $("merge-dlg-keep");
  function close() {
    wrap.hidden = true;
    document.removeEventListener("keydown", onKey, true);
    okBtn.onclick = keepBtn.onclick = null;
    if (prevFocus && prevFocus.focus) prevFocus.focus();
  }
  function onKey(ev) {
    if (ev.key === "Escape") { ev.stopPropagation(); close(); }
    else if (ev.key === "Tab") trapTab(card, ev);
  }
  okBtn.onclick = () => { close(); doMergeAccept(project, path, okBtn); };
  keepBtn.onclick = () => { close(); doMergeDecline(project, path, keepBtn); };
  wrap.hidden = false;
  document.addEventListener("keydown", onKey, true);
  okBtn.focus();
}

async function doFileShare(project, path, btn) {
  // Share = a cairn:// deep link for teammates who already have access
  // to this project. It NEVER uploads bytes anywhere: the old
  // third-party "public link" fallback (upload to file.io) is gone for
  // good - a local-first media tool must not ship the user's footage to
  // a public host behind a button labelled Share. To reach someone
  // WITHOUT Cairn access: Review -> Generate link (token-gated,
  // expiring, revocable), or Download the file and send it yourself.
  if (!path) return;
  const cairnLink = `cairn://${encodeURIComponent(project)}/${encodeURIComponent(path)}`;
  const ok = await copyText(cairnLink);
  toast(ok ? t("toast.sharedLink") : t("toast.denied", { e: t("err.clipboard") }), !ok);
}

/* #44: the menu-item variant of Make available - the menu button has
   already closed, so busy() wraps the (transient) menu item, and the
   toast carries the result. */
async function doRecallWithBusy(project, path, btn) {
  const b = busy(btn, t("busy.avail"));
  try {
    await doRecall(project, path);
  } finally {
    done(b || btn);
  }
}

/* ============================== recent assets (dashboard) ==============================
   The mockup's three-column table: file (type-aware icon), status
   (a green check when synced), pinned (a working pushpin). Rows are the
   newest files of the most recently active project - real rows, not a
   curated list: what you touched is what you see. */

const AUDIO_IC = '<svg class="ic f-ic f-audio" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M6.2 11.4V3.3l6-1.2v8"/><circle cx="4.2" cy="11.6" r="2"/><circle cx="10.2" cy="10.2" r="2"/></svg>';
const FILM_IC = '<svg class="ic f-ic f-film" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.2" stroke-linejoin="round" aria-hidden="true"><rect x="1.8" y="3.2" width="12.4" height="9.6" rx="1.4"/><path d="M4.6 3.2v9.6M11.4 3.2v9.6M1.8 8h12.4M1.8 5.6h2.8M1.8 10.4h2.8M11.4 5.6h2.8M11.4 10.4h2.8"/></svg>';

const AUDIO_EXT = ["wav", "mp3", "aif", "aiff", "flac", "ogg", "m4a", "aac"];
const FILM_EXT = ["braw", "mov", "mp4", "mxf", "r3d", "mkv", "avi", "mpg", "mpeg", "webm"];

function fileIcon(path) {
  const ext = String(path || "").split(".").pop().toLowerCase();
  if (AUDIO_EXT.includes(ext)) return AUDIO_IC;
  if (FILM_EXT.includes(ext)) return FILM_IC;
  return DOC_IC;
}

async function refreshAssetsPanel() {
  const p = recentProject();
  const pid = p ? p.project_id : "";
  if (!pid) { LAST_ASSETS = null; renderRecentAssets(null, ""); return; }
  try {
    LAST_ASSETS = await getJSON(`/api/v1/files?project=${encodeURIComponent(pid)}`);
    renderRecentAssets(LAST_ASSETS, pid);
  } catch { /* the state chip owns daemon-down */ }
}

function renderRecentAssets(r, pid) {
  const files = (r && r.files) || [];
  const sum = (r && r.summary) || {};
  const rows = files
    .slice()
    .sort((a, b) => (b.mtime ?? 0) - (a.mtime ?? 0))
    .slice(0, 8);
  renderIfChanged(
    "assets-body",
    `${pid}:${sigOf(rows, (f) => `${f.path}:${f.size ?? 0}:${f.state ?? ""}:${f.pinned ? 1 : 0}`)}`,
    () => renderRecentAssetsInner(rows, pid),
  );
  const count = $("assets-count");
  if (count) count.textContent = files.length ? String(sum.files ?? files.length) : "";
}

function renderRecentAssetsInner(rows, pid) {
  const body = $("assets-body");
  if (!body) return;
  body.textContent = "";
  if (!rows.length) {
    const tr = document.createElement("tr");
    tr.innerHTML = `<td colspan="4" class="empty">${esc(t("empty.files"))}</td>`;
    body.appendChild(tr);
    return;
  }
  rows.forEach((f) => {
    const [dir, base] = splitPath(f.path);
    const [cls, key] = stFor(f);
    const tr = document.createElement("tr");
    tr.title = esc(f.path);
    // round 27: size next to the name (a 48 GB BRAW row must SAY it),
    // and hover quick-actions: Open reveals it in the OS file manager,
    // Pin keeps it on this machine - the two things a dashboard row is FOR
    tr.innerHTML =
      `<td class="td-file">` +
      `<span class="f-name">${fileIcon(f.path)}<span class="f-base" title="${esc(f.path)}">${esc(base)}</span></span>` +
      (dir ? `<span class="f-dir">${esc(dir)}</span>` : "") +
      `</td>` +
      `<td class="num td-size" title="${esc(t("th.size"))}">${fmtBytes(f.size)}</td>` +
      `<td class="td-status"><span class="f-state s-${cls}" title="${esc(t(key))}">${ST_ICONS[cls]}<span class="sr-only">${esc(t(key))}</span></span></td>` +
      `<td class="td-pin">` +
      `<span class="row-actions">` +
      `<button type="button" class="btn btn-icon btn-qa" data-open="${esc(f.path)}" aria-label="${esc(t("btn.open"))}" title="${esc(t("btn.open"))}">${OPEN_IC}</button>` +
      `<button type="button" class="btn btn-icon btn-qa${f.pinned ? " is-pinned" : ""}" data-pin="${esc(f.path)}" ` +
      `data-pinned="${f.pinned ? "1" : ""}" aria-label="${f.pinned ? esc(t("btn.unpin")) : esc(t("btn.pin"))}" title="${f.pinned ? esc(t("btn.unpin")) : esc(t("btn.pin"))}">${PIN_IC}</button>` +
      `<button type="button" class="btn btn-dots" data-dots="${esc(f.path)}" aria-label="${esc(t("a11y.rowMenu"))} · ${esc(base)}" aria-haspopup="menu" aria-expanded="false" title="${esc(t("a11y.rowMenu"))}">${DOTS_IC}</button>` +
      `</span></td>`;
    tr.addEventListener("click", (ev) => {
      if (ev.target.closest("[data-pin]") || ev.target.closest("[data-open]") || ev.target.closest("[data-dots]")) return;
      jumpToProject(pid);
    });
    const openBtn = tr.querySelector("[data-open]");
    openBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      doFileOpen(pid, f.path);
    });
    const pinBtn = tr.querySelector("[data-pin]");
    pinBtn.addEventListener("click", (ev) => {
      ev.stopPropagation();
      doFilePin(pid, f.path, f.pinned === true);
    });
    const dotsBtnEl = tr.querySelector("[data-dots]");
    dotsBtnEl.addEventListener("click", (ev) => {
      ev.stopPropagation();
      openMenu(dotsBtnEl, fileRowMenuItems(f, pid));
    });
    body.appendChild(tr);
  });
}

/* ============================== feed + activity chart ============================== */

async function refreshFeed() {
  try {
    const feed = await getJSON("/api/v1/feed");
    renderFeed(feed.activity || []);
    renderLocks(feed.leases || []);
  } catch { /* covered by the state chip */ }
}

/* ============================== the activity chart ==============================
   A REAL chart, not a stub: per-day byte totals from /api/v1/activity
   (the daemon buckets by the caller's timezone). Seven slots ending
   today, weekday labels from the locale, a "nice" y axis, a monotone
   cubic line (smooth but never overshooting below a data point), the
   gradient area under it, and hover dots with a value tooltip. */

function niceStep(x) {
  const p = Math.pow(10, Math.floor(Math.log10(Math.max(x, 1e-9))));
  const f = x / p;
  const nf = f <= 1 ? 1 : f <= 2 ? 2 : f <= 2.5 ? 2.5 : f <= 5 ? 5 : 10;
  return nf * p;
}

/* SI (1000-based) compact units for chart ticks and tooltips: the axis
   reads 0 / 50 / 100 kB like the mockup's "0k / 40k / 120k" — clean
   decimal steps, never "48.8 KB" artifacts from mixing nice decimal
   steps with binary divisions. */
function fmtCompact(n) {
  if (!Number.isFinite(n) || n <= 0) return "0";
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} GB`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} MB`;
  if (n >= 1000) return `${Math.round(n / 1000)} kB`;
  return `${Math.round(n)} B`;
}

/* Fritsch-Carlson monotone tangents: the curve stays inside the
   vertical band of its endpoints, so a rising day never fakes a dip */
function monotoneTangents(pts) {
  const n = pts.length;
  if (n === 1) return [0];
  const m = [];
  for (let i = 0; i < n - 1; i++) {
    const dx = pts[i + 1].x - pts[i].x;
    m.push((pts[i + 1].y - pts[i].y) / (dx || 1));
  }
  const t = new Array(n);
  t[0] = m[0];
  t[n - 1] = m[n - 2];
  for (let i = 1; i < n - 1; i++) {
    if (m[i - 1] * m[i] <= 0) {
      t[i] = 0;
    } else {
      const d0 = pts[i].x - pts[i - 1].x;
      const d1 = pts[i + 1].x - pts[i].x;
      const w1 = 2 * d1 + d0;
      const w2 = d1 + 2 * d0;
      t[i] = (w1 + w2) / (w1 / m[i - 1] + w2 / m[i]);
    }
  }
  return t;
}

function monotonePath(pts) {
  if (!pts.length) return "";
  if (pts.length === 1) return `M${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}`;
  const t = monotoneTangents(pts);
  let d = `M${pts[0].x.toFixed(1)},${pts[0].y.toFixed(1)}`;
  for (let i = 0; i < pts.length - 1; i++) {
    const dx = (pts[i + 1].x - pts[i].x) / 3;
    const x0 = pts[i].x + dx, y0 = pts[i].y + t[i] * dx;
    const x1 = pts[i + 1].x - dx, y1 = pts[i + 1].y - t[i + 1] * dx;
    d += `C${x0.toFixed(1)},${y0.toFixed(1)} ${x1.toFixed(1)},${y1.toFixed(1)} ${pts[i + 1].x.toFixed(1)},${pts[i + 1].y.toFixed(1)}`;
  }
  return d;
}

const NS_SVG = "http://www.w3.org/2000/svg";
function svgEl(name, attrs) {
  const el = document.createElementNS(NS_SVG, name);
  for (const k of Object.keys(attrs || {})) el.setAttribute(k, String(attrs[k]));
  return el;
}

function localDayKey(date) {
  return `${date.getFullYear()}-${date.getMonth()}-${date.getDate()}`;
}

async function refreshActivityChart() {
  try {
    const off = new Date().getTimezoneOffset();
    LAST_CHART = await getJSON(`/api/v1/activity?days=7&tz_offset=${off}`);
  } catch { /* keep the last good chart */ }
  renderActivityChart();
}

function renderActivityChart() {
  const box = $("chart-box");
  const note = $("chart-note");
  if (!box) return;
  box.textContent = "";

  const buckets = new Map();
  for (const d of ((LAST_CHART && LAST_CHART.days) || [])) {
    buckets.set(localDayKey(new Date(Number(d.start_ms))), {
      bytes: Number(d.bytes) || 0,
      files: Number(d.files) || 0,
      start: Number(d.start_ms),
    });
  }

  // the 7 slots: local days ending today
  const slots = [];
  const today = new Date();
  today.setHours(0, 0, 0, 0);
  for (let i = 6; i >= 0; i--) {
    const d = new Date(today.getTime() - i * 86400e3);
    const b = buckets.get(localDayKey(d));
    slots.push({ date: d, bytes: b ? b.bytes : 0, files: b ? b.files : 0 });
  }

  if (!buckets.size) {
    box.innerHTML = `<div class="chart-empty">${esc(t("chart.empty"))}</div>`;
    note.textContent = "";
    return;
  }

  const W = 560, H = 150;
  const L = 44, R = 10, T = 12, B = 22;
  const plotW = W - L - R, plotH = H - T - B;
  const max = Math.max(...slots.map((s) => s.bytes), 1);
  const step = niceStep(max / 3);
  const top = Math.max(Math.ceil(max / step) * step, step);
  const y = (v) => T + plotH - (v / top) * plotH;
  const x = (i) => L + (i + 0.5) * (plotW / 7);
  const pts = slots.map((s, i) => ({ x: x(i), y: y(s.bytes), s }));

  const svg = svgEl("svg", {
    viewBox: `0 0 ${W} ${H}`,
    preserveAspectRatio: "none",
    role: "img",
    "aria-label": t("chart.a11y", { peak: fmtCompact(max) }),
  });

  const defs = svgEl("defs", {});
  const grad = svgEl("linearGradient", { id: "chart-grad", x1: 0, y1: 0, x2: 0, y2: 1 });
  grad.appendChild(svgEl("stop", { class: "gs-a", offset: "0" }));
  grad.appendChild(svgEl("stop", { class: "gs-b", offset: "1" }));
  defs.appendChild(grad);
  svg.appendChild(defs);

  // y grid + tick labels
  const grid = svgEl("g", { class: "chart-grid" });
  for (let v = 0; v <= top + 1e-9; v += step) {
    const gy = y(v).toFixed(1);
    grid.appendChild(svgEl("line", {
      x1: L, x2: W - R, y1: gy, y2: gy,
      "stroke-width": v === 0 ? 1.2 : 1,
    }));
    const label = svgEl("text", { class: "chart-tick", x: L - 8, y: (+gy + 3.5).toFixed(1), "text-anchor": "end" });
    label.textContent = fmtCompact(v);
    svg.appendChild(label);
  }
  svg.appendChild(grid);

  // x labels: weekday from the locale (real localization, no table)
  const wdFmt = new Intl.DateTimeFormat(LANG, { weekday: "short" });
  slots.forEach((s, i) => {
    const isToday = i === slots.length - 1;
    const label = svgEl("text", {
      class: "chart-x",
      x: x(i).toFixed(1), y: H - 6, "text-anchor": "middle",
      "font-weight": isToday ? 600 : 500,
    });
    label.textContent = wdFmt.format(s.date);
    svg.appendChild(label);
  });

  // area + line
  const lineD = monotonePath(pts);
  const base = (T + plotH).toFixed(1);
  const areaD =
    `${lineD} L${pts[pts.length - 1].x.toFixed(1)},${base} L${pts[0].x.toFixed(1)},${base} Z`;
  svg.appendChild(svgEl("path", { class: "chart-area", d: areaD }));
  svg.appendChild(svgEl("path", { class: "chart-line", d: lineD }));

  // hover layer: per-day columns, one floating dot + HTML tooltip
  const dot = svgEl("circle", { class: "chart-dot", r: 4, opacity: 0 });
  svg.appendChild(dot);
  const tip = document.createElement("div");
  tip.className = "chart-tip";
  box.appendChild(svg);
  box.appendChild(tip);

  const slotW = plotW / 7;
  const dFmt = new Intl.DateTimeFormat(LANG, { month: "short", day: "numeric" });
  slots.forEach((s, i) => {
    const col = svgEl("rect", { class: "chart-col", x: (x(i) - slotW / 2).toFixed(1), y: T, width: slotW.toFixed(1), height: plotH });
    const hit = svgEl("rect", { class: "chart-hit", x: (x(i) - slotW / 2).toFixed(1), y: T, width: slotW.toFixed(1), height: H });
    hit.addEventListener("mouseenter", () => {
      col.classList.add("is-hot");
      dot.setAttribute("cx", pts[i].x.toFixed(1));
      dot.setAttribute("cy", pts[i].y.toFixed(1));
      dot.setAttribute("opacity", "1");
      tip.innerHTML = `${esc(wdFmt.format(s.date))} ${esc(dFmt.format(s.date))} · <span class="mono">${esc(fmtCompact(s.bytes))}</span>`;
      tip.style.left = `${(pts[i].x / W) * 100}%`;
      tip.style.top = `${(pts[i].y / H) * 100}%`;
      tip.classList.add("is-on");
    });
    hit.addEventListener("mouseleave", () => {
      col.classList.remove("is-hot");
      dot.setAttribute("opacity", "0");
      tip.classList.remove("is-on");
    });
    svg.appendChild(col);
    svg.appendChild(hit);
  });

  note.textContent = t("chart.note");
}


/* ============================== locks ============================== */

function renderLocks(locks) {
  LAST_LOCKS = locks || [];
  const list = $("lock-list");
  if (!list) return;
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
    // review #17: human states ("Editing now" / "Last edited earlier");
    // the technical lease token moves into the title (detail line)
    row.innerHTML =
      `<span class="lock-path" title="${esc(l.path ?? "")}">${esc(l.path ?? "")}</span>` +
      `<span class="tag ${live ? "ok" : "warn"}">${live ? esc(t("lock.editingNow")) : esc(t("lock.earlier"))}</span>` +
      `<span class="lock-when dim mono" title="${esc(l.token ?? "")}">${esc(relTime(l.expires_at && !live ? l.expires_at : now - remainMs))}</span>`;
    list.appendChild(row);
  }
}

/* ============================== team ============================== */

function renderTeam(r) {
  const body = $("team-body");
  if (!body) return;
  LAST_TEAM = r; // Home's computers-connected count reads this
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
  refreshStateRecords();
}

/* ============ People: the synced roster (state-records) ============
   ADR-0031 Phase 1 read surface: GET /api/v1/state-records?family=member
   is the synced record table every computer converges on - the honest
   source for "who is on this project" news (joins, removals), rendered
   as human lines instead of JSON. Tombstoned records read as "was
   removed"; unknown/empty answers render as the quiet empty line. */
let LAST_RECORDS = [];
function renderStateRecords(r) {
  const box = $("records-list");
  if (!box) return;
  LAST_RECORDS = (r && r.records) || [];
  renderIfChanged(
    "records-list",
    sigOf(LAST_RECORDS, (x) => `${x.record_id ?? x.key ?? ""}:${x.ts_ms ?? 0}:${x.tombstone ? 1 : 0}`),
    () => renderStateRecordsInner(),
  );
}
function renderStateRecordsInner() {
  const box = $("records-list");
  if (!box) return;
  box.textContent = "";
  const rows = LAST_RECORDS.slice().sort((a, b) => (b.ts_ms ?? 0) - (a.ts_ms ?? 0)).slice(0, 6);
  if (!rows.length) {
    const li = document.createElement("li");
    li.className = "note";
    li.textContent = t("people.recordsNone");
    box.appendChild(li);
    return;
  }
  for (const rec of rows) {
    const li = document.createElement("li");
    li.className = "record-row";
    li.innerHTML =
      `<span class="tag ${rec.tombstone ? "bad" : "ok"}">${esc(rec.tombstone ? t("people.recordLeft") : t("people.recordJoined"))}</span>` +
      `<span class="sans">${esc(String(rec.key ?? rec.record_id ?? ""))}</span>` +
      `<span class="mono dim">${esc(relTime(rec.ts_ms))}</span>`;
    box.appendChild(li);
  }
}
async function refreshStateRecords() {
  try {
    const r = await getJSON("/api/v1/state-records?family=member");
    renderStateRecords(r);
  } catch { /* the roster news stays at its last honest value */ }
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
    const labeled = !!(s.label && String(s.label).trim());
    const version = labeled ? String(s.label) : t("versions.unlabeled");
    const tr = document.createElement("tr");
    // human version row: the label (or "untitled version") is the row's
    // words; the commit hash is technical detail living in the title
    tr.innerHTML =
      `<td class="sans" title="${esc(s.commit_hash || "")}">${esc(version)}${labeled ? "" : ` <span class="mono dim">${esc((s.commit_hash || "").slice(0, 10))}</span>`}</td>` +
      `<td class="sans dim">${esc(s.author || "")}</td>` +
      `<td class="sans"><button type="button" class="btn btn-ghost btn-sm" data-restore="${esc(s.commit_hash)}">${esc(t("btn.restoreThis"))}</button></td>`;
    tr.querySelector("[data-restore]").addEventListener("click", async (ev) => {
      const project = selectedProject();
      if (!project) return;
      // capture the button BEFORE the first await: currentTarget is only
      // valid while the event dispatches, and the modal yields
      const btn = ev.currentTarget;
      const hash = btn.dataset.restore;
      // #38-#41: the destructive path gets the real modal, not
      // window.confirm - consequences, the version's name and the
      // project's file count, then an explicit "Restore this version".
      // Contract (2-d, landed on its branch): the backend checkpoints the
      // current state BEFORE restoring and answers checkpoint_version -
      // so the dialog may honestly say the current state is saved first,
      // and the success toast names the checkpoint version when present.
      const ok = await confirmDialog({
        title: t("confirm.restoreTitle"),
        body: t("confirm.restoreBody", { label: version, n: FOOT.files ?? 0 }),
        okLabel: t("btn.restoreThis"),
        danger: true,
      });
      if (!ok) return;
      const b = busy(btn, t("busy.restore"));
      try {
        const r = await postJSON("/api/v1/snapshots/restore", {
          project_id: project,
          commit_hash: hash,
        });
        if (r.ok) {
          // the checkpoint note is honest only when the backend sent one
          // (merged 2-d backend); older daemons get the plain restored line
          toast(r.checkpoint_version !== undefined && r.checkpoint_version !== null
            ? t("toast.restoredCkpt", { n: r.restored_files, b: fmtBytes(r.bytes), v: r.checkpoint_version })
            : t("toast.restored", { n: r.restored_files, b: fmtBytes(r.bytes) }));
        }
        else toast(t("toast.failed", { e: r.error }), true);
      } catch (e) {
        toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
      } finally {
        done(b || btn);
      }
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

async function doSnapshot(ev) {
  const project = selectedProject();
  if (!project) return;
  const btn = ev && ev.currentTarget;
  const b = busy(btn, t("busy.saving"));
  try {
    const r = await postJSON("/api/v1/snapshots", {
      project_id: project,
      label: $("snapshot-label").value.trim(),
    });
    if (r.ok) {
      $("snapshot-label").value = "";
      toast(t("toast.versionCreated"));
      refreshSnapshots();
    } else toast(t("toast.failed", { e: r.error }), true);
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally { done(b || btn); }
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

async function doRecall(project, path, btn) {
  const pid = project || selectedProject();
  if (!pid) return;
  const b = busy(btn, t("busy.avail"));
  try {
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
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally { done(b || btn); }
}

/* ============================== flags ============================== */

/* round 27: every flag carries a plain-language description - "when
   one clicks enable it gives explanation as to what it does just
   there, no obnoxious big boxes". The descriptions live in the STR
   dictionary (flags.<name>.line / .flip) so all four languages carry
   them; this list only knows WHICH flags have human copy. */
const FLAG_HELP = [
  "packing_enabled", "tiering_enabled", "delta_fold_enabled", "compression_enabled",
  "placeholder_driver", "normalize_containers", "live_presence", "semantic_merge", "fec_parity",
];
const flagHelp = (name) => ({
  line: STR[`flags.${name}.line`] ? t(`flags.${name}.line`) : "",
  flip: STR[`flags.${name}.flip`] ? t(`flags.${name}.flip`) : "",
});

function renderFlags(flags) {
  const grid = $("flag-grid");
  if (!grid) return;
  renderIfChanged(
    "flag-grid",
    sigOf(flags || [], (f) => `${f.name}:${f.value}`),
    () => renderFlagsInner(flags),
  );
}

function renderFlagsInner(flags) {
  const grid = $("flag-grid");
  grid.innerHTML = "";
  for (const f of flags || []) {
    const on = String(f.value).toLowerCase() !== "false";
    const help = FLAG_HELP.includes(f.name) ? flagHelp(f.name) : { line: "", flip: "" };
    const div = document.createElement("button");
    div.type = "button";
    div.className = "flag" + (on ? " on" : "");
    div.setAttribute("role", "switch");
    div.setAttribute("aria-checked", String(on));
    div.dataset.name = f.name;
    div.dataset.next = on ? "false" : "true";
    div.innerHTML =
      `<span class="flag-name">${esc(f.name)}</span>` +
      `<span class="flag-state">${f.name === "placeholder_driver" ? esc(f.value) : on ? "on" : "off"}</span>` +
      (help.line ? `<span class="flag-desc">${esc(help.line)}</span>` : "");
    div.addEventListener("click", async (ev) => {
      const btn = ev.currentTarget;
      if (btn.disabled) return;
      const b = busy(btn, t("busy.working"));
      try {
        const r = await postJSON("/api/v1/flags", { name: btn.dataset.name, value: btn.dataset.next });
        if (r && r.ok === false) {
          toast(t("toast.denied", { e: r.error }), true);
          return;
        }
        // the explanation lands WITH the action, naming what it just did
        const flippedTo = btn.dataset.next === "true";
        const nextHelp = FLAG_HELP.includes(btn.dataset.name) ? flagHelp(btn.dataset.name) : { flip: "" };
        if (nextHelp.flip && flippedTo) toast(nextHelp.flip);
        else if (!flippedTo) toast(t("flags.off", { name: btn.dataset.name }));
        // refreshAll() repaints the grid with the new state
      } catch (e) {
        toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
      } finally {
        done(b || btn);
      }
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
    LIVE_SSE = new EventSource(withToken("/api/v1/live"));
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
    setLangOpen(false);
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
  if (gPending && ev.key === "h") { showView("howto"); gPending = false; return; }
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
  if (!text) return;
  navigator.clipboard
    .writeText(text)
    .then(() => toast(t("toast.copied")))
    .catch(() => toast(t("toast.denied", { e: t("err.clipboard") }), true));
}

// Delegated copy for dynamically-rendered buttons (review link result,
// connect code, share links). Pixel-perfect rule: every [data-copy]
// copies, no matter when it was added to the DOM.
document.addEventListener("click", (ev) => {
  const btn = ev.target.closest("[data-copy]");
  if (!btn) return;
  // Skip if the button has its own dedicated handler already bound
  // (connect-copy, project card copy) — they call copyText directly.
  if (btn.id === "connect-copy") return;
  const text = btn.dataset.copy || "";
  if (!text) return;
  copyText(text).then((ok) => toast(ok ? t("toast.copied") : t("toast.denied", { e: t("err.clipboard") }), !ok));
});

/* promise-shaped clipboard write (quick-actions await it to report) */
async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch {
    return false;
  }
}

/* ============================== actions ============================== */

async function attachFlow(root, project, btn) {
  if (!root && !project) return;
  const b = busy(btn, t("busy.attach"));
  try {
    const r = await postJSON("/api/v1/attach", { root_path: root, project_id: project });
    if (!r.ok) toast(t("toast.failed", { e: r.error }), true);
    else {
      toast(t("toast.attached"));
      for (const id of ["attach-root", "attach-project", "ob-root"]) {
        const el = $(id);
        if (el) el.value = "";
      }
      closePanels();
    }
  } catch (e) {
    toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e: String(e) }), true);
  } finally { done(b || btn); }
  refreshAll();
}

/* the slide-over + onboarding inputs keep their element-based flow */
async function doAttach(rootEl, projectEl, btn) {
  const root = (rootEl && rootEl.value.trim()) || "";
  if (!root) {
    if (rootEl) rootEl.focus();
    return;
  }
  const project = projectEl ? projectEl.value.trim() : "";
  await attachFlow(root, project, btn);
  if (rootEl) rootEl.value = "";
  if (projectEl) projectEl.value = "";
}

$("btn-attach").addEventListener("click", (ev) => doAttach($("attach-root"), $("attach-project"), ev.currentTarget));
$("attach-cli-copy").addEventListener("click", copyBtn);
$("ob-cli-copy").addEventListener("click", copyBtn);
$("ob-attach-btn").addEventListener("click", (ev) => doAttach($("ob-root"), null, ev.currentTarget));
$("ob-root").addEventListener("keydown", (ev) => {
  if (ev.key === "Enter") { ev.preventDefault(); doAttach($("ob-root"), null); }
});

$("btn-snapshot").addEventListener("click", (ev) => doSnapshot(ev));
$("btn-recall").addEventListener("click", (ev) => doRecall(undefined, undefined, ev.currentTarget));

$("files-filter").addEventListener("input", () => {
  window.clearTimeout(searchTimer);
  searchTimer = window.setTimeout(refreshFiles, 220);
});
$("files-project").addEventListener("change", refreshFiles);

/* ============================== orchestration ============================== */

/* FAST tier (2s): presence + sync truth - the things the user is
   watching while they wait. Status, projects, files, feed, recent
   assets, recall progress, live presence. Cheap GETs over loopback. */
async function refreshFastTier() {
  await refreshStatus();
  await refreshProjects();
  renderOnboarding();
  await refreshFiles();
  await refreshFeed();
  await refreshAssetsPanel();
  await pollRecallJobs();
  const node = $("set-node");
  if (node && DAEMON_UP !== false) node.textContent = t("node.online");
  refreshLive();
}

/* SLOW tier (15s): storage meters, the activity chart, version list,
   feature flags, doctor telemetry. Nothing here changes faster than a
   human notices; polling it at 2s was pure churn (spec item 9). */
let SLOW_REFRESHING = false;
async function refreshSlowTier() {
  if (SLOW_REFRESHING) return;
  SLOW_REFRESHING = true;
  try {
    await refreshStorage();
    await refreshActivityChart();
    await refreshSnapshots();
    try {
      const f = await getJSON("/api/v1/flags");
      renderFlags(f.flags);
    } catch { /* covered */ }
    await refreshOnce();
  } finally {
    SLOW_REFRESHING = false;
  }
}

/* refreshAll = everything, once - used after actions and on tab focus.
   The CADENCE lives in POLL_MS_PLAN below: fast tier at 2s, slow tier
   at 15s, review 5s, team 8s, connect 8s, update 30s. */
let REFRESHING = false;
async function refreshAll() {
  if (REFRESHING) return;
  REFRESHING = true;
  try {
    await refreshFastTier();
    await refreshSlowTier();
    await Promise.allSettled([refreshConnect(), refreshReviewPage()]);
  } finally {
    REFRESHING = false;
  }
}
const POLL_MS_PLAN = [
  [refreshFastTier, 2000],
  [refreshSlowTier, 15000],
  [refreshReview, 5000],
  [refreshTeam, 8000],
  [refreshUpdate, 30000],
  [refreshConnect, 8000],
];

/* re-render the dynamic surfaces after a language switch */
function rerenderAllDynamic() {
  renderOnboarding();
  paintState();
  renderHero();
  renderRecentSession();
  renderActiveProjects();
  renderActions();
  renderProjectsSettings();
  renderRecentAssets(LAST_ASSETS, (recentProject() || {}).project_id || "");
  renderLocks(LAST_LOCKS);
  renderStateRecords({ records: LAST_RECORDS });
  renderReview(LAST_REVIEW);
  renderFiles(LAST_FILES);
  renderRecallJobs();
  renderActivityChart();
  refreshTeam();
  refreshSnapshots();
  refreshSlowTier();
}

/* view from the URL hash on boot (deep links still land) */
(function bootRoute() {
  const h = (location.hash || "").replace("#", "");
  showView(VIEWS.includes(h) ? h : "dashboard");
})();

/* card cascade indexes */
stagger(document.querySelector(".dash-grid"), ".card");
stagger(document.querySelector(".settings-col"), ".card");
stagger(document.querySelector(".howto-grid"), ".card");

let POLL_TIMERS = [];
function startPolling() {
  if (POLL_TIMERS.length) return;
  POLL_TIMERS = POLL_MS_PLAN.map(([fn, ms]) => setInterval(fn, ms));
}
function stopPolling() {
  POLL_TIMERS.forEach(clearInterval);
  POLL_TIMERS = [];
}
document.addEventListener("visibilitychange", () => {
  if (document.hidden) {
    stopPolling();
  } else {
    startPolling();
    refreshAll();
  }
});

/* boot */
applyI18n();
refreshTeam();
refreshReview();
refreshUpdate();
/* refreshAll covers fast + slow tiers + connect/review pages once, so
   the first paint has every surface populated */
refreshAll();
startPolling();
/* cadence lives in ONE place: the POLL_MS_PLAN owner above. */
/* ============ CONNECT, REVIEW, MERGE wiring (round 27 prime real estate) ============ */
async function refreshConnect() {
  try {
    // NOTE: not `t` - that name is the i18n function and shadowing it here
    // made every t() call below throw (the old code used literal strings).
    const team = await getJSON("/api/v1/team");
    const code = team.join_code || (team.projects && team.projects[0] && team.projects[0].join_code) || "";
    const sig = team.signal || (team.projects && team.projects[0] && team.projects[0].signal) || "";
    const swarm = team.swarm || "";
    const codeEl = document.getElementById("connect-code");
    const hint = document.getElementById("connect-hint");
    const sigEl = document.getElementById("connect-signal");
    const sigAddr = document.getElementById("connect-signal-addr");
    const swarmEl = document.getElementById("connect-swarm");
    if (codeEl) { codeEl.textContent = code || "-"; codeEl.dataset.copy = code || ""; document.getElementById("connect-copy").dataset.copy = code || ""; }
    if (hint) hint.textContent = code ? t("connect.hintCode") : t("connect.hintNone");
    if (sigEl) sigEl.textContent = sig ? t("connect.online") : t("connect.offline");
    if (sigAddr) sigAddr.textContent = sig || t("connect.noSignal");
    if (swarmEl) swarmEl.textContent = swarm || t("connect.noSwarm");
  } catch (e) { /* team not ready */ }
}
async function refreshReviewPage() {
  try {
    const r = await getJSON("/api/v1/review");
    const entry = (r.review && r.review[0]) || null;
    const list = document.getElementById("review-list");
    const linksWrap = document.getElementById("review-links");
    const notes = document.getElementById("review-notes");
    const count = document.getElementById("review-notes-count");
    if (list) {
      // r.review is PER-PROJECT entries; the old code iterated the array
      // as if each entry were a version ("vundefined" rows). Versions
      // live one level down.
      const versions = (entry && entry.versions) || [];
      if (!versions.length) list.innerHTML = `<p class="note">${esc(t("review.noVersions"))}</p>`;
      else {
        list.innerHTML = "";
        for (const rev of versions) {
          // human version metadata (review round): "Version 3 · label" is
          // the row's words; duration/frames/fps live in the quiet detail
          // line underneath
          const label = rev.label || entry.title || t("review.untitled");
          const fps = rev.fps_num && rev.fps_den ? `${rev.fps_num}/${rev.fps_den}` : "?";
          const div = document.createElement("div");
          div.className = "review-version";
          div.style.cssText = "padding:8px;border:1px solid var(--hairline);border-radius:8px;margin:4px 0";
          const head = document.createElement("div");
          head.className = "review-version-title sans";
          head.textContent = t("review.vTitle", { n: rev.number, label });
          const detail = document.createElement("div");
          detail.className = "review-version-detail mono dim";
          detail.style.cssText = "font-size:0.82em;margin-top:2px";
          detail.textContent = t("review.vDetails", { d: rev.duration || "?", frames: rev.frames ?? "?", fps });
          div.append(head, detail);
          list.appendChild(div);
        }
      }
    }
    // links you can SEE are links you can kill: expiry date, copy, revoke
    if (linksWrap) {
      const links = (entry && entry.links) || [];
      linksWrap.innerHTML = "";
      for (const l of links) {
        const when = l.expired ? t("review.expired")
          : l.expires_at > 0 ? t("review.expires", { d: new Date(l.expires_at).toLocaleDateString() })
          : t("review.never");
        const row = document.createElement("div");
        row.style.cssText = "display:flex;gap:6px;align-items:center;padding:4px 0;flex-wrap:wrap";
        row.innerHTML =
          `<span class="mono" style="font-size:0.85em">…${esc(String(l.token || "").slice(-6))}</span>` +
          `<span class="note" style="margin:0">${esc(l.note || "")} · ${esc(l.role || "")} · ${esc(when)}</span>` +
          // 2-a additive field: the link was revoked on ANOTHER computer
          // and the tombstone has synced here - say so before anyone
          // copies a link that now answers like an unknown token
          (l.revoked_remotely ? `<span class="tag bad">${esc(t("review.revokedRemote"))}</span>` : "") +
          `<span style="flex:1"></span>` +
          `<button type="button" class="btn btn-ghost btn-sm" data-copy="${esc(location.origin + "/r/" + (l.token || ""))}">${esc(t("btn.copy"))}</button>` +
          (l.expired ? "" : `<button type="button" class="btn btn-ghost btn-sm" data-revoke="${esc(l.token || "")}">${esc(t("review.revoke"))}</button>`);
        linksWrap.appendChild(row);
      }
    }
    if (count && notes) { count.textContent = "0"; }
    refreshReviewMedia();
  } catch {}
}

/* the publish picker: the USER chooses which cut, not "first .mp4".
   Placeholders stay out - a 0-byte stub has nothing to watch. */
const REVIEW_MEDIA_RE = /\.(mp4|mov|m4v|mkv|avi|webm|mpg|mpeg|mxf|r3d|braw)$/i;
let REVIEW_MEDIA_PID = "";
async function refreshReviewMedia() {
  const sel = document.getElementById("review-media-select");
  if (!sel) return;
  const project = selectedProject();
  if (!project) { sel.innerHTML = ""; REVIEW_MEDIA_PID = ""; return; }
  if (REVIEW_MEDIA_PID === project && sel.options.length) return; // stable
  try {
    const files = await getJSON(`/api/v1/files?project=${encodeURIComponent(project)}`);
    const media = (files.files || []).filter((f) => !f.placeholder && REVIEW_MEDIA_RE.test(f.path));
    sel.innerHTML = "";
    for (const f of media) {
      const o = document.createElement("option");
      o.value = f.path;
      o.textContent = f.path;
      sel.appendChild(o);
    }
    REVIEW_MEDIA_PID = project;
  } catch { /* files not ready; the next cycle retries */ }
}
// wire connect buttons
document.getElementById("connect-copy")?.addEventListener("click", async (ev) => {
  const c = ev.currentTarget.dataset.copy || document.getElementById("connect-code")?.textContent || "";
  if (!c || c === "-") return toast(t("connect.noCode"), true);
  const ok = await copyText(c);
  toast(ok ? t("connect.copied") : t("toast.denied", { e: t("err.clipboard") }), !ok);
});
document.getElementById("connect-regenerate")?.addEventListener("click", async (ev) => {
  const btn = ev.currentTarget;
  const b = busy(btn, t("busy.generatingCode"));
  try {
    const r = await postJSON("/api/v1/team/regenerate", {});
    toast(r.ok ? t("connect.generated", { c: r.join_code || r.code }) : t("toast.failed", { e: r.error || "unknown" }), !r.ok);
    refreshConnect();
  } catch (e) { toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e }), true); }
  finally { done(b || btn); }
});
document.getElementById("connect-join")?.addEventListener("click", async (ev) => {
  const btn = ev.currentTarget;
  const code = document.getElementById("connect-input")?.value?.trim();
  if (!code) return toast(t("connect.joinFirst"), true);
  const b = busy(btn, t("busy.joining"));
  try {
    const r = await postJSON("/api/v1/team/join", { code });
    const out = document.getElementById("connect-join-result");
    // honest-join (review #3): the backend records intent and returns the
    // real next step - the enroll + login commands. Saying "Joined!" there
    // was a UX lie; show the saved + next-step truth instead.
    const joinedMsg = r.ok ? t("connect.joined") + " " + (r.next || "") : t("connect.joinFailed", { e: r.error || "unknown" });
    if (out) out.textContent = joinedMsg;
    toast(joinedMsg, !r.ok);
    if (r.ok) refreshAll();
  } catch (e) { toast(e && e.message === "timeout" ? t("toast.timeout") : t("connect.joinFailed", { e }), true); }
  finally { done(b || btn); }
});
// wire review buttons
document.getElementById("review-link-create")?.addEventListener("click", async (ev) => {
  const btn = ev.currentTarget;
  const note = document.getElementById("review-link-note")?.value || "Client";
  const role = document.getElementById("review-link-role")?.value || "commenter";
  // expiry chooser (contract freeze item 2): the select's value IS the
  // ttl in hours - 168/720/2160 = 7/30/90 days; the backend defaults to
  // 720 when the key is absent
  const ttl = Number(document.getElementById("review-link-ttl")?.value) || 720;
  const project = selectedProject();
  const out = document.getElementById("review-link-result");
  const b = busy(btn, t("busy.link"));
  try {
    const r = await postJSON("/api/v1/review/link", { project_id: project, note, role, ttl_hours: ttl });
    if (out) {
      if (r.ok) {
        const full = `${location.origin}${r.link}`;
        const expires = new Date(Date.now() + ttl * 3600e3).toLocaleDateString(LANG);
        // the spec'd result card: "✓ Link ready · <note> · expires <date>"
        // + [Copy link] + who can open it. No auto-copy: the button is
        // the explicit act (clipboard permission prompts surprise people).
        out.innerHTML =
          `<p class="review-link-ok">✓ ${esc(t("review.linkReady"))} · ${esc(note)} · ${esc(t("review.linkExpiresOn", { d: expires }))}</p>` +
          `<div style="display:flex;gap:8px;align-items:center;flex-wrap:wrap"><code class="mono" style="word-break:break-all">${esc(full)}</code>` +
          `<button type="button" class="btn btn-ghost btn-sm" data-copy="${esc(full)}">${esc(t("review.linkCopy"))}</button></div>` +
          `<p class="note" style="margin:6px 0 0">${esc(t("review.linkAnyone"))}</p>`;
        refreshReviewPage();
      } else {
        out.textContent = t("toast.failed", { e: r.error });
      }
    }
  } catch (e) { toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e }), true); }
  finally { done(b || btn); }
});
document.getElementById("review-publish")?.addEventListener("click", async (ev) => {
  const btn = ev.currentTarget;
  const project = selectedProject();
  if (!project) return toast(t("connect.needProject"), true);
  const media = document.getElementById("review-media-select")?.value;
  if (!media) return toast(t("review.noMedia"), true);
  const b = busy(btn, t("busy.publish"));
  try {
    // no frames/fps here ON PURPOSE: the backend probes the media file
    // and owns the truth (a guessed fps is a wrong timecode for every
    // reviewer downstream)
    const r = await postJSON("/api/v1/review/publish", { project_id: project, media });
    toast(r.ok ? t("review.published", { n: r.version }) : t("toast.failed", { e: r.error }), !r.ok);
    if (r.ok) refreshReviewPage();
  } catch (e) { toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e }), true); }
  finally { done(b || btn); }
});
// revoke a guest link (delegated: the rows re-render every refresh)
document.getElementById("review-links")?.addEventListener("click", async (ev) => {
  const btn = ev.target.closest("[data-revoke]");
  if (!btn || btn.disabled) return;
  const b = busy(btn, t("busy.working"));
  try {
    const r = await postJSON("/api/v1/review/revoke", { token: btn.dataset.revoke });
    toast(r.ok ? t("review.revoked") : t("toast.failed", { e: r.error }), !r.ok);
    if (r.ok) refreshReviewPage();
  } catch (e) { toast(e && e.message === "timeout" ? t("toast.timeout") : t("toast.failed", { e }), true); }
  finally { done(b || btn); }
});
// wire merge
document.getElementById("merge-run")?.addEventListener("click", async (ev) => {
  const btn = ev.currentTarget;
  const base = document.getElementById("merge-base")?.files?.[0];
  const ours = document.getElementById("merge-ours")?.files?.[0];
  const theirs = document.getElementById("merge-theirs")?.files?.[0];
  const semantic = document.getElementById("merge-semantic")?.checked;
  const out = document.getElementById("merge-output");
  if (!base || !ours || !theirs) { if (out) out.textContent = t("merge.pickThree"); return; }
  if (out) out.textContent = t("merge.running");
  const b = busy(btn, t("busy.merge"));
  try {
    const read = (f) => f.text();
    // NOTE: not `[b, o, t]` - that shadowed the i18n function inside this
    // try block (the old code only got away with it by using literals).
    const [baseText, o, th] = await Promise.all([read(base), read(ours), read(theirs)]);
    const r = await postJSON("/api/v1/tl-merge", { base_otio: baseText, ours_otio: o, theirs_otio: th, semantic: !!semantic });
    if (out) out.textContent = JSON.stringify(r, null, 2);
    toast(r.ok ? t("merge.outcome", { o: r.outcome || r.policy || "done" }) : t("toast.failed", { e: r.error }), !r.ok);
  } catch (e) {
    if (out) out.textContent = `cairn tl-merge --base ${base.name} --ours ${ours.name} --theirs ${theirs.name}${semantic ? " --semantic" : ""}\n\n${t("merge.fallback", { e })}`;
  } finally { done(b || btn); }
});
document.getElementById("merge-search-btn")?.addEventListener("click", async () => {
  const q = document.getElementById("merge-search")?.value?.trim();
  if (!q) return;
  const out = document.getElementById("merge-search-out");
  if (out) out.textContent = t("merge.searching");
  try {
    const r = await getJSON(`/api/v1/search?q=${encodeURIComponent(q)}`);
    if (out) out.innerHTML = `<pre class="mono" style="background:var(--subtle);padding:8px;border-radius:8px;white-space:pre-wrap">${JSON.stringify(r, null, 2)}</pre>`;
  } catch { const r2 = await getJSON(`/api/v1/files?project=${PROJECTS[0]?.project_id||""}`).then(j=>j.files||[]).catch(()=>[]); const hits = r2.filter(f=>f.path.includes(q)).map(f=>f.path).join("\n"); if (out) out.textContent = hits || t("merge.noMatches"); }
});
