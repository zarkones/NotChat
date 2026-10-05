package dev.dioxus.main

import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent

/** Restart the foreground service after reboot or after an app update (adb install -r). */
class NotChatBootReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        when (intent.action) {
            Intent.ACTION_BOOT_COMPLETED,
            Intent.ACTION_MY_PACKAGE_REPLACED -> {
                NotChatBridge.init(context)
                NotChatBridge.startService(context)
                NotChatBridge.scheduleBackupJob(context)
            }
        }
    }
}
