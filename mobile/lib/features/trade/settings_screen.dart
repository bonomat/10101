import 'package:flutter/material.dart';
import 'package:get_10101/features/trade/trade_screen.dart';
import 'package:get_10101/bridge_generated/bridge_definitions.dart' as bridge;
import 'package:get_10101/common/settings_screen.dart';

class TradeSettingsScreen extends StatelessWidget {
  static const route = "${TradeScreen.route}/$subRouteName";
  static const subRouteName = "settings";
  final bridge.Config config;

  const TradeSettingsScreen({super.key, required this.config});

  @override
  Widget build(BuildContext context) {
    return SettingsScreen(fromRoute: route, config: config);
  }
}
