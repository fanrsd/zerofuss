package com.zerofuss.bridge;

import android.app.Activity;
import android.content.Intent;
import android.hardware.usb.UsbAccessory;
import android.hardware.usb.UsbManager;
import android.os.Bundle;
import android.os.Handler;
import android.os.Looper;
import android.os.ParcelFileDescriptor;
import android.view.Gravity;
import android.view.WindowManager;
import android.widget.TextView;

import java.io.FileInputStream;
import java.io.FileOutputStream;
import java.io.InputStream;
import java.io.OutputStream;
import java.net.Inet4Address;
import java.net.InetAddress;
import java.net.NetworkInterface;
import java.net.ServerSocket;
import java.net.Socket;
import java.util.Collections;

/**
 * Jembatan AOA: goggles (USB host) -> HP ini (USB accessory) -> PC lewat TCP.
 *
 * Byte diteruskan mentah dua arah, tanpa ditafsirkan. Arah PC->goggles dipakai
 * untuk mengirim handshake yang nanti kita rekayasa dari sisi desktop.
 */
public class MainActivity extends Activity {

    private static final int PORT = 8888;

    private TextView view;
    private final Handler ui = new Handler(Looper.getMainLooper());

    private ParcelFileDescriptor fd;
    private InputStream fromGoggles;
    private OutputStream toGoggles;

    private volatile String accName = "-";
    private volatile String peer = "-";
    private volatile long rx, tx;

    @Override
    protected void onCreate(Bundle saved) {
        super.onCreate(saved);
        view = new TextView(this);
        view.setTextSize(16f);
        view.setGravity(Gravity.CENTER);
        setContentView(view);
        getWindow().addFlags(WindowManager.LayoutParams.FLAG_KEEP_SCREEN_ON);

        new Thread(this::serveForever, "tcp-server").start();
        ui.post(this.refresh);
        openAccessory(getIntent());
    }

    @Override
    protected void onNewIntent(Intent intent) {
        super.onNewIntent(intent);
        setIntent(intent);
        openAccessory(intent);
    }

    private final Runnable refresh = new Runnable() {
        @Override
        public void run() {
            view.setText("Zerofuss Bridge\n\n"
                    + "accessory : " + accName + "\n"
                    + "PC        : " + peer + "\n"
                    + "listen    : " + localIp() + ":" + PORT + "\n\n"
                    + "goggles -> PC : " + rx + " B\n"
                    + "PC -> goggles : " + tx + " B");
            ui.postDelayed(this, 500);
        }
    };

    private void openAccessory(Intent intent) {
        UsbManager usb = (UsbManager) getSystemService(USB_SERVICE);
        UsbAccessory acc = intent == null ? null : intent.getParcelableExtra(UsbManager.EXTRA_ACCESSORY);
        if (acc == null) {
            UsbAccessory[] list = usb.getAccessoryList();
            if (list != null && list.length > 0) acc = list[0];
        }
        if (acc == null) {
            accName = "belum ada (colok goggles)";
            return;
        }
        fd = usb.openAccessory(acc);
        if (fd == null) {
            accName = "izin ditolak / gagal dibuka";
            return;
        }
        accName = acc.getManufacturer() + " / " + acc.getModel();
        fromGoggles = new FileInputStream(fd.getFileDescriptor());
        toGoggles = new FileOutputStream(fd.getFileDescriptor());
    }

    /** Satu klien pada satu waktu; cukup untuk bench. */
    private void serveForever() {
        try (ServerSocket server = new ServerSocket(PORT)) {
            while (true) {
                Socket sock = server.accept();
                sock.setTcpNoDelay(true);
                peer = sock.getInetAddress().getHostAddress();
                rx = tx = 0;
                Thread up = new Thread(() -> pump(sock), "pc-to-goggles");
                up.start();
                drain(sock);
                sock.close();
                peer = "-";
            }
        } catch (Exception e) {
            peer = "server mati: " + e;
        }
    }

    /** goggles -> PC */
    private void drain(Socket sock) {
        byte[] buf = new byte[16384];
        try {
            OutputStream out = sock.getOutputStream();
            while (fromGoggles != null) {
                int n = fromGoggles.read(buf);
                if (n < 0) break;
                out.write(buf, 0, n);
                rx += n;
            }
        } catch (Exception ignored) {
        }
    }

    /** PC -> goggles */
    private void pump(Socket sock) {
        byte[] buf = new byte[4096];
        try {
            InputStream in = sock.getInputStream();
            while (toGoggles != null) {
                int n = in.read(buf);
                if (n < 0) break;
                toGoggles.write(buf, 0, n);
                toGoggles.flush();
                tx += n;
            }
        } catch (Exception ignored) {
        }
    }

    private static String localIp() {
        try {
            for (NetworkInterface ni : Collections.list(NetworkInterface.getNetworkInterfaces())) {
                if (ni.isLoopback() || !ni.isUp()) continue;
                for (InetAddress a : Collections.list(ni.getInetAddresses())) {
                    if (a instanceof Inet4Address) return a.getHostAddress();
                }
            }
        } catch (Exception ignored) {
        }
        return "?";
    }
}
