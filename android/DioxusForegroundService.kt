package dev.solochain.node

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.os.Build
import android.os.IBinder

/**
 * Foreground service that pins the app process so the solochain node child
 * process keeps running while the app is backgrounded (Android freezes/kills
 * cached app process groups otherwise).
 *
 * Started/stopped from Rust over JNI via the static helpers below
 * (Intent.setClassName avoids needing a Class reference from native code).
 */
class DioxusForegroundService : Service() {

    override fun onBind(intent: Intent?): IBinder? = null

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        createChannel()
        val notification: Notification =
            Notification.Builder(this, CHANNEL_ID)
                .setContentTitle("Solochain node")
                .setContentText("Node running — RPC ws://127.0.0.1:9944")
                .setSmallIcon(android.R.drawable.stat_notify_sync)
                .setOngoing(true)
                .build()

        if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
            startForeground(NOTIF_ID, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_DATA_SYNC)
        } else {
            startForeground(NOTIF_ID, notification)
        }
        // NOT sticky: if the system kills the process group, the node child is
        // gone too — a restarted service would show a lying notification.
        return START_NOT_STICKY
    }

    private fun createChannel() {
        val nm = getSystemService(NotificationManager::class.java) ?: return
        val channel = NotificationChannel(
            CHANNEL_ID,
            "Node status",
            NotificationManager.IMPORTANCE_LOW, // silent, no heads-up popup
        )
        nm.createNotificationChannel(channel)
    }

    companion object {
        private const val CHANNEL_ID = "solochain_node"
        private const val NOTIF_ID = 0x5010

        @JvmStatic
        fun start(context: Context) {
            val intent = Intent(context, DioxusForegroundService::class.java)
            if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
                context.startForegroundService(intent)
            } else {
                context.startService(intent)
            }
        }

        @JvmStatic
        fun stop(context: Context) {
            context.stopService(Intent(context, DioxusForegroundService::class.java))
        }
    }
}
