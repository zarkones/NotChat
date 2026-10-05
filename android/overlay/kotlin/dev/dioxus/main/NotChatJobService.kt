package dev.dioxus.main

import android.app.job.JobParameters
import android.app.job.JobService
import android.os.Handler
import android.os.Looper

/**
 * Periodic (15 min, persisted) backup — JobScheduler, i.e. what WorkManager
 * uses underneath, without adding a dependency.
 *
 * If the foreground service is alive this is a no-op. If the OEM killed it, we
 * try to restart it (allowed from background only with an exemption such as
 * "battery: unrestricted"); either way the Rust runtime is started in-process
 * and the job holds the process for ~8 minutes so the onion can come up,
 * receive pending messages and drain the outbox.
 */
class NotChatJobService : JobService() {
    private val handler = Handler(Looper.getMainLooper())

    override fun onStartJob(params: JobParameters): Boolean {
        NotChatBridge.init(this)
        if (NotChatService.running) return false
        NotChatBridge.startService(this)
        NotChatBridge.startRust(this)
        handler.postDelayed({ jobFinished(params, false) }, 8 * 60 * 1000L)
        return true
    }

    override fun onStopJob(params: JobParameters): Boolean {
        handler.removeCallbacksAndMessages(null)
        return true
    }
}
