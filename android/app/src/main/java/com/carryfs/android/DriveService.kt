package com.carryfs.android

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.hardware.usb.UsbManager
import android.os.Build
import android.util.Log
import kotlin.concurrent.thread

/**
 * Runs in the foreground while a USB drive is mounted, so Android keeps the
 * process and its mounts alive. Its notification ejects every drive, and it
 * forgets drives that are unplugged.
 */
class DriveService : Service() {
    companion object {
        private const val EJECT = "com.carryfs.android.EJECT"
        private const val CHANNEL = "drives"
    }

    private val unplugged = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            val device = intent.usbDevice() ?: return
            Drives.detached(context, device)
        }
    }

    override fun onCreate() {
        super.onCreate()
        val filter = IntentFilter(UsbManager.ACTION_USB_DEVICE_DETACHED)
        if (Build.VERSION.SDK_INT >= 33) {
            registerReceiver(unplugged, filter, RECEIVER_NOT_EXPORTED)
        } else {
            registerReceiver(unplugged, filter)
        }
    }

    override fun onDestroy() {
        unregisterReceiver(unplugged)
        super.onDestroy()
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        startForeground()
        if (intent?.action == EJECT) {
            thread {
                for (drive in Drives.all()) {
                    try {
                        Drives.eject(this, drive.key)
                    } catch (e: Exception) {
                        Log.e("CarryFS", "Ejecting ${drive.name} failed", e)
                    }
                }
            }
        }
        return START_NOT_STICKY
    }

    override fun onBind(intent: Intent?) = null

    private fun startForeground() {
        val manager = getSystemService(NotificationManager::class.java)
        manager.createNotificationChannel(
            NotificationChannel(CHANNEL, getString(R.string.channel_name), NotificationManager.IMPORTANCE_LOW),
        )
        val eject = PendingIntent.getService(
            this, 0, Intent(this, DriveService::class.java).setAction(EJECT), PendingIntent.FLAG_IMMUTABLE,
        )
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        val notification = Notification.Builder(this, CHANNEL)
            .setSmallIcon(R.drawable.ic_cube)
            .setContentTitle(getString(R.string.notification_title))
            .setContentText(getString(R.string.notification_text))
            .setContentIntent(open)
            .setOngoing(true)
            .addAction(Notification.Action.Builder(null, getString(R.string.eject_all), eject).build())
            .build()
        if (Build.VERSION.SDK_INT >= 29) {
            startForeground(1, notification, ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE)
        } else {
            startForeground(1, notification)
        }
    }
}
