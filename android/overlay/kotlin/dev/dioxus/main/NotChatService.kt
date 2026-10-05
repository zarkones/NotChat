package dev.dioxus.main

import android.app.Service
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder
import android.util.Log

/**
 * Sticky foreground service ("NotChat is running", min-priority notification).
 *
 * It does no networking itself: it keeps the *process* in the foreground-service
 * state so Android neither freezes nor kills it and keeps network access in
 * Doze. The Rust runtime (Arti onion service + /v1 HTTP accept loop + outbox
 * retry loop, src/onion.rs) lives on its own threads in this same process and
 * is started here too, so it also runs after boot / when no Activity exists.
 */
class NotChatService : Service() {
    companion object {
        @Volatile @JvmStatic var running: Boolean = false
    }

    override fun onCreate() {
        super.onCreate()
        NotChatBridge.init(this)
        goForeground()
        running = true
        NotChatBridge.startRust(this)
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        goForeground()
        running = true
        NotChatBridge.startRust(this) // idempotent
        return START_STICKY
    }

    private fun goForeground() {
        val n = NotChatBridge.buildServiceNotification(this)
        try {
            if (Build.VERSION.SDK_INT >= 34) {
                startForeground(
                    NotChatBridge.SERVICE_NOTIF_ID, n,
                    ServiceInfo.FOREGROUND_SERVICE_TYPE_REMOTE_MESSAGING
                )
            } else {
                startForeground(NotChatBridge.SERVICE_NOTIF_ID, n)
            }
        } catch (t: Throwable) {
            Log.e("NotChat", "startForeground failed", t)
        }
    }

    override fun onTaskRemoved(rootIntent: Intent?) {
        // Swiping NotChat away from Recents keeps the service (and the onion) up.
        super.onTaskRemoved(rootIntent)
    }

    override fun onDestroy() {
        running = false
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null
}
