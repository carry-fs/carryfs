package com.carryfs.android

import android.content.Context
import android.content.Intent
import android.hardware.usb.UsbConstants
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbDeviceConnection
import android.hardware.usb.UsbInterface
import android.hardware.usb.UsbManager
import android.os.Build
import android.os.Handler
import android.os.HandlerThread
import android.os.Looper
import android.provider.DocumentsContract
import java.io.File
import java.io.IOException
import java.util.concurrent.ConcurrentHashMap

/** Mounted drives, shared by the activity, the service, and the provider. */
object Drives {
    /** The drive key of the debug disk image. USB drives use their device IDs. */
    const val IMAGE = -1L

    class Drive(
        val key: Long,
        val name: String,
        val volumes: List<Long>,
        private val connection: UsbDeviceConnection?,
        private val iface: UsbInterface?,
    ) {
        val usb get() = connection != null
        private val worker = lazy { HandlerThread("carryfs-io-$key").apply { start() } }
        val callbacks get() = Handler(worker.value.looper)
        @Volatile var status = emptyMap<Long, VolumeStatus>()
            private set

        fun refresh() {
            status = volumes.associateWith { volume ->
                VolumeStatus(
                    runCatching { Native.label(volume) }.getOrDefault("CarryFS"),
                    runCatching { Native.space(volume) }.getOrNull(),
                    runCatching { Native.needsUpgrade(volume) }.getOrDefault(false),
                    runCatching { Native.readOnly(volume) }.getOrDefault(true),
                )
            }
        }

        fun release() {
            if (worker.isInitialized()) worker.value.quitSafely()
            connection?.releaseInterface(iface)
            connection?.close()
        }
    }

    data class VolumeStatus(val name: String, val space: LongArray?, val legacy: Boolean, val readOnly: Boolean)

    private val drives = ConcurrentHashMap<Long, Drive>()
    private val driveLocks = ConcurrentHashMap<Long, Any>()
    private fun lock(key: Long) = driveLocks.computeIfAbsent(key) { Any() }

    fun owner(volume: Long): Drive? = all().firstOrNull { volume in it.volumes }
    private val main = Handler(Looper.getMainLooper())

    /** Called on the main thread after any drive is mounted or released. */
    var onChange: (() -> Unit)? = null

    fun all(): List<Drive> = drives.values.toList()

    fun volumes(): List<Long> = drives.values.flatMap { it.volumes }

    fun massStorage(device: UsbDevice): UsbInterface? =
        (0 until device.interfaceCount).map(device::getInterface).firstOrNull {
            it.interfaceClass == UsbConstants.USB_CLASS_MASS_STORAGE &&
                it.interfaceSubclass == 6 && it.interfaceProtocol == 0x50
        }

    /** Claim the drive from the kernel and mount its CarryFS volumes. Blocks on I/O. */
    fun mount(context: Context, device: UsbDevice) = synchronized(lock(device.deviceId.toLong())) {
        val key = device.deviceId.toLong()
        if (drives.containsKey(key)) return@synchronized
        val iface = massStorage(device) ?: throw IOException("Not a USB mass-storage device")
        val bulk = (0 until iface.endpointCount).map(iface::getEndpoint)
            .filter { it.type == UsbConstants.USB_ENDPOINT_XFER_BULK }
        val input = bulk.firstOrNull { it.direction == UsbConstants.USB_DIR_IN }
        val output = bulk.firstOrNull { it.direction == UsbConstants.USB_DIR_OUT }
        if (input == null || output == null) throw IOException("USB drive has no bulk endpoints")
        val usb = context.getSystemService(UsbManager::class.java)
        val connection = usb.openDevice(device) ?: throw IOException("Cannot open the USB drive")
        val drive = Drive(key, device.displayName(), emptyList(), connection, iface)
        try {
            // Forcing detaches the kernel driver, unmounting any FAT volume Android mounted.
            if (!connection.claimInterface(iface, true)) throw IOException("Cannot claim the USB drive")
            // A UAS-capable drive may be left in its UAS setting; Bulk-Only Transport is setting 0.
            if (!connection.setInterface(iface)) throw IOException("Cannot select the drive's mass-storage mode")
            val volumes = Native.mountUsb(key, connection.fileDescriptor, iface.id, input.address, output.address)
            if (volumes.isEmpty()) throw IOException("No CarryFS volume on ${drive.name}")
            drives[key] = Drive(key, drive.name, volumes.toList(), connection, iface).apply { refresh() }
        } catch (e: Exception) {
            drive.release()
            throw e
        }
        changed(context)
    }

    /** Mount a disk image from the app's files, for testing without USB. */
    fun mountImage(context: Context, image: File) = synchronized(lock(IMAGE)) {
        if (drives.containsKey(IMAGE)) return@synchronized
        val volumes = Native.mountImage(IMAGE, image.path)
        if (volumes.isEmpty()) throw IOException("No CarryFS volume in ${image.name}")
        drives[IMAGE] = Drive(IMAGE, image.name, volumes.toList(), null, null).apply { refresh() }
        changed(context)
    }

    /** Publish and release a drive. It is released even if publishing fails. */
    fun eject(context: Context, key: Long) = synchronized(lock(key)) {
        val drive = drives.remove(key) ?: return@synchronized
        try {
            Native.eject(key)
        } finally {
            drive.release()
            changed(context)
        }
    }

    /** Make a CarryFS 0.1.0 volume writable. */
    fun upgrade(context: Context, volume: Long) {
        val drive = owner(volume) ?: throw IOException("Drive disconnected")
        synchronized(lock(drive.key)) {
            Native.upgrade(volume)
            drive.refresh()
            changed(context)
        }
    }

    /** Forget a USB drive that was unplugged before it was ejected. */
    fun detached(context: Context, device: UsbDevice) = synchronized(lock(device.deviceId.toLong())) {
        val key = device.deviceId.toLong()
        val drive = drives.remove(key) ?: return@synchronized
        Native.detach(key)
        drive.release()
        changed(context)
    }

    private fun changed(context: Context) {
        val app = context.applicationContext
        main.post {
            app.contentResolver.notifyChange(DocumentsContract.buildRootsUri(CarryProvider.AUTHORITY), null)
            val service = Intent(app, DriveService::class.java)
            // Read the current drives on the main thread so concurrent changes cannot reorder service starts and stops.
            if (drives.values.any { it.usb }) app.startForegroundService(service) else app.stopService(service)
            onChange?.invoke()
        }
    }
}

/** The USB device a system USB intent carries. */
fun Intent.usbDevice(): UsbDevice? = if (Build.VERSION.SDK_INT >= 33) {
    getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java)
} else {
    @Suppress("DEPRECATION")
    getParcelableExtra(UsbManager.EXTRA_DEVICE)
}

/** A name to show: the product, else the maker, else the device path. Some enclosures report blanks. */
fun UsbDevice.displayName(): String =
    listOfNotNull(productName, manufacturerName).map { it.trim() }.firstOrNull { it.isNotEmpty() } ?: deviceName
