import 'package:flutter/gestures.dart';
import 'package:flutter/material.dart';
import 'package:font_awesome_flutter/font_awesome_flutter.dart';
import 'package:get_10101/common/dlc_channel_change_notifier.dart';
import 'package:get_10101/common/poll_widget.dart';
import 'package:get_10101/common/secondary_action_button.dart';
import 'package:get_10101/features/wallet/balance.dart';
import 'package:get_10101/features/wallet/receive_screen.dart';
import 'package:get_10101/features/wallet/scanner_screen.dart';
import 'package:get_10101/features/wallet/wallet_change_notifier.dart';
import 'package:get_10101/util/poll_change_notified.dart';
import 'package:go_router/go_router.dart';
import 'package:provider/provider.dart';

class WalletScreen extends StatelessWidget {
  static const route = "/wallet";
  static const label = "Wallet";

  const WalletScreen({Key? key}) : super(key: key);

  @override
  Widget build(BuildContext context) {
    final pollChangeNotifier = context.watch<PollChangeNotifier>();
    final walletChangeNotifier = context.watch<WalletChangeNotifier>();
    final hasChannel = context.read<DlcChannelChangeNotifier>().hasDlcChannel();

    return Scaffold(
      body: RefreshIndicator(
        onRefresh: () async {
          await walletChangeNotifier.refreshWalletInfo();
          await walletChangeNotifier.waitForSyncToComplete();
          await pollChangeNotifier.refresh();
        },
        child: Container(
          margin: const EdgeInsets.only(top: 7.0),
          padding: const EdgeInsets.symmetric(horizontal: 20),
          child: Column(
            crossAxisAlignment: CrossAxisAlignment.stretch,
            children: [
              const Balance(),
              const SizedBox(height: 10.0),
              Container(
                  margin: const EdgeInsets.only(left: 0, right: 0),
                  child: Row(children: [
                    Expanded(
                      child: SecondaryActionButton(
                        onPressed: () {
                          context.go((hasChannel || walletChangeNotifier.offChain().sats > 0)
                              ? ReceiveScreen.route
                              :
                              // TODO: we should have a dedicated on-boarding screen for on-boarding with on-chain funds
                              ReceiveScreen.route);
                        },
                        icon: FontAwesomeIcons.arrowDown,
                        title: 'Receive',
                      ),
                    ),
                    const SizedBox(width: 10.0),
                    Expanded(
                        child: SecondaryActionButton(
                      onPressed: () => GoRouter.of(context).go(ScannerScreen.route),
                      icon: FontAwesomeIcons.arrowUp,
                      title: 'Send',
                    ))
                  ])),
              const SizedBox(
                height: 10,
              ),
              Expanded(
                child: ScrollConfiguration(
                  behavior: ScrollConfiguration.of(context).copyWith(
                    dragDevices: PointerDeviceKind.values.toSet(),
                  ),
                  child: SingleChildScrollView(
                    physics: const AlwaysScrollableScrollPhysics(),
                    child: Column(
                      children: [
                        const Padding(
                          padding: EdgeInsets.only(bottom: 8.0),
                          child: PollWidget(),
                        ),
                        Card(
                          margin: const EdgeInsets.all(0.0),
                          elevation: 1,
                          child: Column(
                            children: walletChangeNotifier.walletInfo.history
                                .map((e) => e.toWidget())
                                .toList(),
                          ),
                        ),
                      ],
                    ),
                  ),
                ),
              ),
            ],
          ),
        ),
      ),
    );
  }
}
