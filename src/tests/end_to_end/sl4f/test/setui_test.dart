// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

import 'package:sl4f/sl4f.dart' as sl4f;
import 'package:test/test.dart';

const _timeout = Timeout(Duration(minutes: 1));

void main() {
  sl4f.Sl4f sl4fDriver;
  sl4f.SetUi setUi;

  setUp(() async {
    sl4fDriver = sl4f.Sl4f.fromEnvironment();
    await sl4fDriver.startServer();

    setUi = sl4f.SetUi(sl4fDriver);
  });

  tearDown(() async {
    await sl4fDriver.stopServer();
    sl4fDriver.close();
  });

  group(sl4f.SetUi, () {
    test('talks to SL4F getDevNetworkOption without error', () async {
      // If anything throws an exception then we've failed.
      await setUi.getDevNetworkOption();
    });
    test('talks to SL4F setDevNetworkOption no reboot', () async {
      // If anything throws an exception then we've failed.
      await setUi.setDevNetworkOption(sl4f.NetworkOption.ethernet);
    });

    test('get intl', () async {
      final intlInfo = await setUi.getLocale();
      expect(intlInfo, isNotNull);
    });

    test('set Locale', () async {
      final originalInfo = await setUi.getLocale();

      await setUi.setLocale('he-FR');
      var intlInfo = await setUi.getLocale();
      expect(intlInfo.locales.contains('he-FR'), true);

      await setUi.setLocale('zh-TW');
      intlInfo = await setUi.getLocale();
      expect(intlInfo.locales.contains('zh-TW'), true);

      // Restore the original locale, to avoid confusion when this test gets run
      // on a developer's device.
      await setUi.setLocale(originalInfo.locales.first);
    });
  }, timeout: _timeout);
}
