// NotChat Android glue — overlaid onto the dx-generated Gradle project by
// android/build-debug-apk.sh (see ANDROID.md § Background & notifications).
package dev.dioxus.main

import android.Manifest
import android.annotation.SuppressLint
import android.app.Activity
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.job.JobInfo
import android.app.job.JobScheduler
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.content.pm.PackageManager
import android.net.Uri
import android.os.Build
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import androidx.core.content.ContextCompat
import dev.zarkones.onion_chat.R
import java.lang.ref.WeakReference

/**
 * Static bridge between Rust (JNI, see src/android.rs) and Android.
 *
 * Notification rule: every inbound message posts a notification unless the
 * user is *looking at that chat*, i.e. [activePeer] == peer (set from Rust
 * when the Dioxus screen changes) AND the Activity is resumed
 * ([activityResumed], maintained by MainActivity onResume/onPause).
 */
object NotChatBridge {
    private const val TAG = "NotChat"

    const val CH_MESSAGES = "notchat_messages"
    const val CH_REQUESTS = "notchat_requests"
    const val CH_SERVICE = "notchat_service"
    const val SERVICE_NOTIF_ID = 1
    const val JOB_ID = 4711
    const val EXTRA_OPEN_PEER = "dev.zarkones.onion_chat.OPEN_PEER"
    private const val REQ_POST_NOTIFICATIONS = 4242
    private const val MAX_LINES = 6

    @Volatile var activityResumed: Boolean = false
        private set
    @Volatile private var activePeer: String? = null
    @Volatile private var pendingOpenPeer: String? = null
    @Volatile private var appContext: Context? = null
    @Volatile private var serviceStatus: String = "Connecting… (auto-retry)"
    private var activityRef: WeakReference<Activity>? = null
    private val history = HashMap<String, ArrayDeque<String>>()
    @Volatile private var nativeLoaded = false

    // ---- JNI (implemented in src/android.rs) ----
    @JvmStatic external fun nativeInit(ctx: Context)
    @JvmStatic external fun nativeStartBackground(ctx: Context): Boolean

    // ---- lifecycle helpers ----

    @JvmStatic
    fun init(ctx: Context) {
        val app = ctx.applicationContext ?: ctx
        if (appContext == null) appContext = app
        ensureChannels(app)
    }

    private fun loadNative(): Boolean {
        if (nativeLoaded) return true
        return try {
            System.loadLibrary("main")
            nativeLoaded = true
            true
        } catch (t: Throwable) {
            Log.e(TAG, "loadLibrary(main) failed", t)
            false
        }
    }

    /** Start the Rust onion/HTTP/outbox runtime in this process (idempotent). */
    @JvmStatic
    fun startRust(ctx: Context): Boolean {
        init(ctx)
        if (!loadNative()) return false
        return try {
            nativeStartBackground(ctx.applicationContext ?: ctx)
        } catch (t: Throwable) {
            Log.e(TAG, "nativeStartBackground failed", t)
            false
        }
    }

    /** Start (or keep) the sticky foreground service. Safe to call repeatedly. */
    @JvmStatic
    fun startService(ctx: Context): Boolean {
        return try {
            ContextCompat.startForegroundService(ctx, Intent(ctx, NotChatService::class.java))
            true
        } catch (t: Throwable) {
            // Android 12+: ForegroundServiceStartNotAllowedException when started
            // from the background without an exemption (e.g. from the job).
            Log.w(TAG, "startForegroundService refused: $t")
            false
        }
    }

    @JvmStatic
    fun scheduleBackupJob(ctx: Context) {
        try {
            val js = ctx.getSystemService(Context.JOB_SCHEDULER_SERVICE) as JobScheduler
            if (js.getPendingJob(JOB_ID) != null) return
            val info = JobInfo.Builder(JOB_ID, ComponentName(ctx, NotChatJobService::class.java))
                .setPeriodic(15 * 60 * 1000L)
                .setRequiredNetworkType(JobInfo.NETWORK_TYPE_ANY)
                .setPersisted(true)
                .build()
            js.schedule(info)
        } catch (t: Throwable) {
            Log.w(TAG, "scheduleBackupJob failed: $t")
        }
    }

    @JvmStatic
    fun onActivityCreated(activity: Activity, intent: Intent?) {
        activityRef = WeakReference(activity)
        init(activity)
        // libmain is already loaded by WryActivity's companion init.
        nativeLoaded = true
        try {
            nativeInit(activity.applicationContext)
        } catch (t: Throwable) {
            Log.e(TAG, "nativeInit failed", t)
        }
        handleIntent(intent)
        askNotificationPermission(activity)
        startService(activity)
        scheduleBackupJob(activity)
    }

    @JvmStatic
    fun onActivityResumed(activity: Activity) {
        activityRef = WeakReference(activity)
        activityResumed = true
        activePeer?.let { cancelPeer(it) }
    }

    @JvmStatic
    fun onActivityPaused() {
        activityResumed = false
    }

    @JvmStatic
    fun handleIntent(intent: Intent?) {
        val peer = intent?.getStringExtra(EXTRA_OPEN_PEER) ?: return
        intent.removeExtra(EXTRA_OPEN_PEER)
        pendingOpenPeer = peer
    }

    private fun askNotificationPermission(activity: Activity) {
        if (Build.VERSION.SDK_INT >= 33 &&
            activity.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) {
            activity.requestPermissions(
                arrayOf(Manifest.permission.POST_NOTIFICATIONS),
                REQ_POST_NOTIFICATIONS
            )
        }
    }

    // ---- channels / notifications ----

    private fun ensureChannels(ctx: Context) {
        if (Build.VERSION.SDK_INT < 26) return
        val nm = ctx.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager
        if (nm.getNotificationChannel(CH_MESSAGES) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CH_MESSAGES, "Messages", NotificationManager.IMPORTANCE_HIGH)
                    .apply { description = "New NotChat messages" }
            )
        }
        if (nm.getNotificationChannel(CH_REQUESTS) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CH_REQUESTS, "Contact requests", NotificationManager.IMPORTANCE_DEFAULT)
                    .apply { description = "Someone scanned your invite" }
            )
        }
        if (nm.getNotificationChannel(CH_SERVICE) == null) {
            nm.createNotificationChannel(
                NotificationChannel(CH_SERVICE, "Background connection", NotificationManager.IMPORTANCE_MIN)
                    .apply {
                        description = "Keeps NotChat reachable while in the background"
                        setShowBadge(false)
                    }
            )
        }
    }

    private fun canNotify(ctx: Context): Boolean {
        if (Build.VERSION.SDK_INT >= 33 &&
            ctx.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
            PackageManager.PERMISSION_GRANTED
        ) return false
        return NotificationManagerCompat.from(ctx).areNotificationsEnabled()
    }

    private fun notifIdFor(peer: String): Int = 1000 + ((peer.hashCode() and 0x7fffffff) % 1_000_000)

    private fun openIntent(ctx: Context, peer: String?, requestCode: Int): PendingIntent {
        val i = Intent(ctx, MainActivity::class.java).apply {
            action = Intent.ACTION_MAIN
            addCategory(Intent.CATEGORY_LAUNCHER)
            flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP or
                Intent.FLAG_ACTIVITY_CLEAR_TOP
            if (peer != null) putExtra(EXTRA_OPEN_PEER, peer)
        }
        var flags = PendingIntent.FLAG_UPDATE_CURRENT
        if (Build.VERSION.SDK_INT >= 23) flags = flags or PendingIntent.FLAG_IMMUTABLE
        return PendingIntent.getActivity(ctx, requestCode, i, flags)
    }

    /** Called from Rust for every newly stored inbound message. */
    @JvmStatic
    @SuppressLint("MissingPermission")
    fun postMessage(peerId: String, title: String, body: String) {
        val ctx = appContext ?: return
        if (activityResumed && peerId == activePeer) return // user is in that chat
        if (!canNotify(ctx)) return
        val lines = synchronized(history) {
            val q = history.getOrPut(peerId) { ArrayDeque() }
            q.addLast(body)
            while (q.size > MAX_LINES) q.removeFirst()
            q.toList()
        }
        val id = notifIdFor(peerId)
        val b = NotificationCompat.Builder(ctx, CH_MESSAGES)
            .setSmallIcon(R.drawable.ic_stat_notchat)
            .setContentTitle(title)
            .setContentText(body)
            .setCategory(NotificationCompat.CATEGORY_MESSAGE)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .setContentIntent(openIntent(ctx, peerId, id))
            .setWhen(System.currentTimeMillis())
            .setShowWhen(true)
        if (lines.size > 1) {
            val style = NotificationCompat.InboxStyle()
            lines.forEach { style.addLine(it) }
            style.setSummaryText("${lines.size} new messages")
            b.setStyle(style).setNumber(lines.size)
        } else {
            b.setStyle(NotificationCompat.BigTextStyle().bigText(body))
        }
        try {
            NotificationManagerCompat.from(ctx).notify(id, b.build())
        } catch (t: Throwable) {
            Log.w(TAG, "notify failed: $t")
        }
    }

    /** Called from Rust when a new contact request arrives. */
    @JvmStatic
    @SuppressLint("MissingPermission")
    fun postContactRequest(peerId: String, title: String) {
        val ctx = appContext ?: return
        if (activityResumed) return // Chats list shows requests inline
        if (!canNotify(ctx)) return
        val id = notifIdFor("req:$peerId")
        val n = NotificationCompat.Builder(ctx, CH_REQUESTS)
            .setSmallIcon(R.drawable.ic_stat_notchat)
            .setContentTitle("Contact request")
            .setContentText("$title wants to chat")
            .setCategory(NotificationCompat.CATEGORY_SOCIAL)
            .setAutoCancel(true)
            .setContentIntent(openIntent(ctx, peerId, id))
            .build()
        try {
            NotificationManagerCompat.from(ctx).notify(id, n)
        } catch (t: Throwable) {
            Log.w(TAG, "notify failed: $t")
        }
    }

    @JvmStatic
    fun cancelPeer(peerId: String) {
        val ctx = appContext ?: return
        synchronized(history) { history.remove(peerId) }
        NotificationManagerCompat.from(ctx).cancel(notifIdFor(peerId))
    }

    /** Called from Rust when the Dioxus screen changes (null = not in a chat). */
    @JvmStatic
    fun setActivePeer(peerId: String?) {
        activePeer = peerId
        if (peerId != null && activityResumed) cancelPeer(peerId)
    }

    /** Polled by the Rust UI: peer whose notification was tapped (consumed once). */
    @JvmStatic
    fun takePendingOpenPeer(): String? {
        val p = pendingOpenPeer
        pendingOpenPeer = null
        return p
    }

    fun buildServiceNotification(ctx: Context): android.app.Notification {
        init(ctx)
        return NotificationCompat.Builder(ctx, CH_SERVICE)
            .setSmallIcon(R.drawable.ic_stat_notchat)
            .setContentTitle("NotChat is running")
            .setContentText(serviceStatus)
            .setOngoing(true)
            .setSilent(true)
            .setShowWhen(false)
            .setPriority(NotificationCompat.PRIORITY_MIN)
            .setCategory(NotificationCompat.CATEGORY_SERVICE)
            .setForegroundServiceBehavior(NotificationCompat.FOREGROUND_SERVICE_IMMEDIATE)
            .setContentIntent(openIntent(ctx, null, SERVICE_NOTIF_ID))
            .build()
    }

    /** Called from Rust when the onion goes up/down. */
    @JvmStatic
    @SuppressLint("MissingPermission")
    fun setServiceStatus(text: String) {
        serviceStatus = text
        val ctx = appContext ?: return
        if (!NotChatService.running) return
        try {
            NotificationManagerCompat.from(ctx).notify(SERVICE_NOTIF_ID, buildServiceNotification(ctx))
        } catch (t: Throwable) {
            Log.w(TAG, "service notif update failed: $t")
        }
    }

    // ---- Settings helpers (called from Rust UI) ----

    @JvmStatic
    fun backgroundStatus(): String {
        val ctx = appContext ?: return "Unknown"
        val notif = if (canNotify(ctx)) "allowed" else "OFF — you will not see new messages"
        val pm = ctx.getSystemService(Context.POWER_SERVICE) as PowerManager
        val battery = if (Build.VERSION.SDK_INT >= 23 && pm.isIgnoringBatteryOptimizations(ctx.packageName))
            "unrestricted" else "optimized (Android may stop NotChat in background)"
        val svc = if (NotChatService.running) "running" else "not running"
        return "Notifications: $notif\nBattery: $battery\nBackground service: $svc"
    }

    @JvmStatic
    @SuppressLint("BatteryLife")
    fun requestBatteryExemption(): Boolean {
        val ctx = appContext ?: return false
        if (Build.VERSION.SDK_INT < 23) return false
        val act = activityRef?.get()
        return try {
            val pm = ctx.getSystemService(Context.POWER_SERVICE) as PowerManager
            val intent = if (!pm.isIgnoringBatteryOptimizations(ctx.packageName)) {
                Intent(Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS)
                    .setData(Uri.parse("package:${ctx.packageName}"))
            } else {
                Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                    .setData(Uri.parse("package:${ctx.packageName}"))
            }
            if (act != null) act.startActivity(intent)
            else ctx.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            true
        } catch (t: Throwable) {
            Log.w(TAG, "battery exemption intent failed: $t")
            false
        }
    }

    @JvmStatic
    fun requestNotificationPermission(): Boolean {
        val ctx = appContext ?: return false
        val act = activityRef?.get()
        return try {
            if (Build.VERSION.SDK_INT >= 33 && act != null &&
                ctx.checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) !=
                PackageManager.PERMISSION_GRANTED &&
                act.shouldShowRequestPermissionRationale(Manifest.permission.POST_NOTIFICATIONS)
            ) {
                act.requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_POST_NOTIFICATIONS)
            } else {
                val intent = if (Build.VERSION.SDK_INT >= 26) {
                    Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
                        .putExtra(Settings.EXTRA_APP_PACKAGE, ctx.packageName)
                } else {
                    Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS)
                        .setData(Uri.parse("package:${ctx.packageName}"))
                }
                if (act != null) act.startActivity(intent)
                else ctx.startActivity(intent.addFlags(Intent.FLAG_ACTIVITY_NEW_TASK))
            }
            true
        } catch (t: Throwable) {
            Log.w(TAG, "notification settings intent failed: $t")
            false
        }
    }
}
