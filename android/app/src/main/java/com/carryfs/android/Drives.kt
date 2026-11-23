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
import android.os.Looper
import android.provider.DocumentsContract
import java.io.File
import java.io.IOException

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
        val mountWarning: String = "",
    ) {
        val usb get() = connection != null

        fun release() {
            connection?.releaseInterface(iface)
            connection?.close()
        }
    }

    private val drives = LinkedHashMap<Long, Drive>()
    private val main = Handler(Looper.getMainLooper())

    /** Called on the main thread after any drive is mounted or released. */
    var onChange: (() -> Unit)? = null

    @Synchronized fun all(): List<Drive> = drives.values.toList()

    @Synchronized fun volumes(): List<Long> = drives.values.flatMap { it.volumes }

    fun massStorage(device: UsbDevice): UsbInterface? =
        (0 until device.interfaceCount).map(device::getInterface).firstOrNull {
            it.interfaceClass == UsbConstants.USB_CLASS_MASS_STORAGE &&
                it.interfaceSubclass == 6 && it.interfaceProtocol == 0x50
        }

    /** Claim the drive from the kernel and mount its CarryFS volumes. Blocks on I/O. */
    @Synchronized fun mount(context: Context, device: UsbDevice) {
        val key = device.deviceId.toLong()
        if (key in drives) return
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
            drives[key] = Drive(key, drive.name, volumes.toList(), connection, iface, Native.takeMountWarning(key))
        } catch (e: Exception) {
            drive.release()
            throw e
        }
        changed(context)
    }

    /** Mount a disk image from the app's files, for testing without USB. */
    @Synchronized fun mountImage(context: Context, image: File) {
        if (IMAGE in drives) return
        val volumes = Native.mountImage(IMAGE, image.path)
        if (volumes.isEmpty()) throw IOException("No CarryFS volume in ${image.name}")
        drives[IMAGE] = Drive(IMAGE, image.name, volumes.toList(), null, null, Native.takeMountWarning(IMAGE))
        changed(context)
    }

    /** Release the drive only after all its volumes publish successfully. */
    @Synchronized fun eject(context: Context, key: Long) {
        val drive = drives[key] ?: return
        Native.eject(key)
        drives.remove(key)
        drive.release()
        changed(context)
    }

    /** Make a CarryFS 0.1.0 volume writable. */
    @Synchronized fun upgrade(context: Context, volume: Long) {
        Native.upgrade(volume)
        changed(context)
    }

    /** Forget a USB drive that was unplugged before it was ejected. */
    @Synchronized fun detached(context: Context, device: UsbDevice) {
        val key = device.deviceId.toLong()
        val drive = drives.remove(key) ?: return
        Native.detach(key)
        drive.release()
        changed(context)
    }

    private fun changed(context: Context) {
        val app = context.applicationContext
        app.contentResolver.notifyChange(DocumentsContract.buildRootsUri(CarryProvider.AUTHORITY), null)
        val service = Intent(app, DriveService::class.java)
        // The service keeps the process, and so the mounts, alive while a USB drive is in use.
        if (drives.values.any { it.usb }) app.startForegroundService(service) else app.stopService(service)
        onChange?.let { main.post(it) }
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
