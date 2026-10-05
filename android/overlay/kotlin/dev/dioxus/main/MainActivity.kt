package dev.dioxus.main

import android.content.Intent
import android.os.Bundle

typealias BuildConfig = dev.zarkones.onion_chat.BuildConfig

/**
 * NotChat override of the dx-generated MainActivity (overlay, see
 * android/build-debug-apk.sh). Adds: resumed/paused tracking for notification
 * suppression, notification-tap routing, POST_NOTIFICATIONS prompt, and starts
 * the foreground service + backup job.
 */
class MainActivity : WryActivity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        NotChatBridge.onActivityCreated(this, intent)
    }

    override fun onNewIntent(intent: Intent) {
        super.onNewIntent(intent)
        setIntent(intent)
        NotChatBridge.handleIntent(intent)
    }

    override fun onResume() {
        super.onResume()
        NotChatBridge.onActivityResumed(this)
    }

    override fun onPause() {
        NotChatBridge.onActivityPaused()
        super.onPause()
    }
}
